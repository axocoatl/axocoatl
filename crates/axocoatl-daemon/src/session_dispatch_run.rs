//! One-shot native actor execution beneath the owned Session controller.
//! Host-resolved resources remain subject to exact retained authority.
use super::*;
use axocoatl_actor::{
    ActivationCheckpointPort, AgentBehavior, AgentRunOutcome, DefaultAgentBehavior,
};
use axocoatl_core::{AgentConfig, AgentInput, AgentRole, MeasuredTokenUsage, TokenUsageStats};
use axocoatl_llm::{LlmProvider, ProviderExecutionBounds};
use axocoatl_memory::{activation_state::ReservedActivationCheckpoint, AgentCheckpoint};
use axocoatl_session::control_authority::{
    ProviderCallClaim, ProviderCallIntent, ProviderCallOutcome, ProviderUsage,
};
use axocoatl_session::execution_content::{
    ActivationOutputContent, ActivationOutputLimits, DurableActivationOutputReservation,
    DurableReservedExecutionOutput, ExecutionUsage, OutputKind,
};
use axocoatl_token::TokenCounter;
use axocoatl_tools::ToolExecutor;

/// Tool-call rounds whose output an Agent's next request keeps verbatim;
/// older output is replaced by a placeholder. Re-sending every earlier tool
/// result each round is what exhausts a small local model's context.
const KEPT_TOOL_ROUNDS: usize = 3;

/// Host-resolved resources. This first port supports an autonomous in-process
/// actor with retained text inputs. Repository runs use the separate opaque
/// RepositoryActivationResource; binary attachment projection still requires
/// its own validated resource port. Helpers are admitted through `delegate`.
pub struct AutonomousActivationResources {
    pub config: AgentConfig,
    pub profile: ExecutionProfile,
    pub provider: Arc<dyn LlmProvider>,
    pub counter: Arc<dyn TokenCounter>,
    pub tools: Arc<ToolExecutor>,
}

/// Not Clone or deserializable. Storage is reserved before this handle exists.
/// Dropping it closes dispatch; neither this handle nor recovery replays work.
pub struct PreparedActivation {
    controller: SessionDispatchController,
    activation: ActivationRef,
    config: AgentConfig,
    input: AgentInput,
    behavior: Option<Box<dyn AgentBehavior>>,
    control: AgentRunControl,
    port: Arc<CheckpointPort>,
    output: DurableActivationOutputReservation,
    settled: bool,
    // Last: behavior/provider/checkpoint fields must drop before ownership ends.
    _execution: super::execution_lifetime::ExecutionTicket,
}

/// Evidence retained for one run. `accepted` requires both the complete output
/// and the actual candidate to be selected by the canonical journal.
pub struct SettledActivation {
    pub activation: ActivationRef,
    pub output: DurableReservedExecutionOutput,
    pub checkpoint: Option<CheckpointRef>,
    pub accepted: bool,
    pub failure: Option<String>,
}

impl SessionDispatchController {
    /// Incurred observations, including incomplete calls, remain available
    /// independently of whether a checkpoint or answer was accepted.
    pub fn activation_provider_usage(&self, activation: &ActivationRef) -> Result<ProviderUsage> {
        self.lock()?
            .authority
            .provider_usage(activation)
            .map_err(error)
    }

    pub fn prepare_autonomous_activation(
        &self,
        activation: ActivationRef,
        resources: AutonomousActivationResources,
    ) -> Result<PreparedActivation> {
        self.prepare_autonomous_with_repository(activation, resources, None)
    }

    /// Run the unchanged native actor with an exact retained Session checkout.
    /// The host-resolved resource replaces caller-supplied tools; descriptive
    /// repository metadata alone can never enable filesystem or process access.
    pub fn prepare_repository_activation(
        &self,
        activation: ActivationRef,
        resources: AutonomousActivationResources,
        repository: RepositoryActivationResource,
    ) -> Result<PreparedActivation> {
        self.prepare_autonomous_with_repository(activation, resources, Some(repository))
    }

    fn prepare_autonomous_with_repository(
        &self,
        activation: ActivationRef,
        resources: AutonomousActivationResources,
        repository: Option<RepositoryActivationResource>,
    ) -> Result<PreparedActivation> {
        let AutonomousActivationResources {
            config,
            profile,
            provider,
            counter,
            tools,
        } = resources;
        if !matches!(
            config.role,
            AgentRole::Autonomous | AgentRole::Coordinator | AgentRole::Worker
        ) || profile.isolation != "in-process"
            || config.provider != profile.provider
            || config.model != profile.model
            || config.tools != profile.tools
            || config.writes != profile.write_scope
            || provider.provider_id() != profile.provider
            || provider.model_id() != profile.model
        {
            return Err(error(
                "autonomous resources differ from the admitted execution profile",
            ));
        }
        let mut state = self.lock()?;
        state.execution_admission()?;
        if config.role == AgentRole::Worker
            && state.native_child_origin(&activation.node_id)?.is_none()
        {
            return Err(error(
                "a Worker runs only as a helper admitted by its lead's delegate call",
            ));
        }
        if state.bound.contains_key(&activation.activation_id) {
            return Err(error(
                "activation already has an executor; never bind it twice",
            ));
        }
        let snapshot = state.current(&activation)?;
        let manifest = &snapshot
            .contract()
            .activations()
            .iter()
            .find(|item| item.activation == activation)
            .unwrap()
            .input;
        let resolved = state
            .content
            .validate_input(&snapshot, manifest)
            .map_err(error)?;
        let (_, request) = state
            .content
            .retained_request(&state.turn_id)
            .map_err(error)?
            .ok_or_else(|| error("canonical request body is unavailable"))?;
        if config.id.0 != manifest.conversation_id.as_str() {
            return Err(error(
                "actor identity differs from the admitted conversation",
            ));
        }
        // The native actor persists content, not its auxiliary context field.
        // Project retained evidence before acquiring an executor or reservation,
        // so a missing/unsupported input cannot turn into a reduced request.
        repository_activation::validate_input_resource(&state, manifest, repository.as_ref())?;
        let input = match repository.as_ref() {
            Some(resource) => super::input::project_repository_input(
                manifest,
                snapshot.request_ref().unwrap(),
                request,
                &resolved,
                resource,
            )?,
            None => super::input::project_text_input(
                manifest,
                snapshot.request_ref().unwrap(),
                request,
                &resolved,
            )?,
        };
        let tools = match repository.as_ref() {
            Some(resource) => resource.preview_tools(&profile)?,
            None => tools,
        };
        let control = state.child_run_control(&activation)?;
        let configuration = serde_json::to_string(&config).map_err(error)?;
        // Expected refusal is local to this activation. Validate before any
        // reservation write; uncertain persistence still fences the controller.
        let grant = manifest
            .grant
            .as_ref()
            .ok_or_else(|| error("activation has no exact grant"))?;
        let policy = state
            .authority
            .grant_policy(grant.grant_id.as_str())
            .map_err(error)?;
        if profile.definition != manifest.definition.definition_id.as_str() {
            return Err(error(
                "input grant or definition differs from current authority",
            ));
        }
        state.validate_physical_input(
            manifest,
            &profile,
            &configuration,
            &policy,
            repository.as_ref(),
        )?;
        if let ConversationSavepoint::Checkpoint { checkpoint } = &manifest.starting_savepoint {
            state.memory.checkpoint(checkpoint).map_err(error)?;
        }
        for parent in &manifest.parents {
            state.memory.checkpoint(&parent.checkpoint).map_err(error)?;
        }
        let admitted_at_ms = now_ms()?;
        state
            .authority
            .validate_activation_grant(
                &activation,
                grant.grant_id.as_str(),
                &profile,
                admitted_at_ms,
            )
            .map_err(error)?;
        // This exact generation has not been registered yet. Every prior
        // generation must already have durable accounting coverage.
        state.conversation_usage_excluding(&manifest.conversation_id, Some(&activation))?;
        let result = (|| {
            let mut bound = state.bind_with_repository(
                activation.clone(),
                profile.clone(),
                &configuration,
                control.clone(),
                true,
                admitted_at_ms,
                repository,
            )?;
            let checkpoint = {
                let DispatchState {
                    memory, canonical, ..
                } = &mut *state;
                memory
                    .reserve_candidate(canonical, &activation)
                    .map_err(error)?
            };
            let output = state
                .content
                .reserve_activation_output(
                    &snapshot,
                    &activation,
                    ActivationOutputLimits {
                        partial_records: 0,
                        partial_bytes: 0,
                        settlement_bytes: MAX_RESULT_BYTES,
                    },
                )
                .map_err(error)?;
            // Verify accounting while admission is still synchronous. Missing
            // historical journals or older tool-only runs cannot mean zero.
            state.conversation_usage(&manifest.conversation_id)?;
            bound.steering_open = true;
            state.bound.insert(activation.activation_id.clone(), bound);
            Ok((checkpoint, output))
        })();
        let (checkpoint, output) = state.fail_closed(result)?;
        let execution = state.acquire_execution_ticket(self)?;
        let hooks = state.hooks.clone();
        drop(state);
        let control = control.with_execution_boundary(Arc::new(ActivationBoundary {
            controller: self.clone(),
            activation: activation.clone(),
        }));
        // A read-only helper is never offered the file-writing tools. Its
        // stored definition keeps them; authority refuses them regardless.
        let offered_tools: Vec<String> = if profile.write_scope.as_ref().is_some_and(Vec::is_empty)
        {
            config
                .tools
                .iter()
                .filter(|tool| !matches!(tool.as_str(), "write_file" | "edit_file"))
                .cloned()
                .collect()
        } else {
            config.tools.clone()
        };
        let port = Arc::new(CheckpointPort {
            controller: self.clone(),
            reservation: checkpoint,
            candidate: Mutex::new(None),
        });
        let provider = Arc::new(super::provider::SessionProvider::new(
            self.clone(),
            activation.clone(),
            provider,
            profile.provider,
            profile.model,
        ));
        let observer = Arc::new(super::stream::ActivationStreamObserver::new(
            self.clone(),
            activation.clone(),
        ));
        let host_delegate_tool = self.scoped_delegate_tool(&activation)?;
        let host_knowledge_tool = self.scoped_knowledge_tool(&activation)?;
        // A Coordinator template runs here as a lead like any other Agent: its
        // approved Worker templates are reachable only through `delegate`.
        let mut behavior = DefaultAgentBehavior::new(provider, counter)
            .with_tool_round_limit(policy.limits.invocations)
            .with_tool_executor(tools)
            .with_executor_tool_allowlist(offered_tools)
            .with_activation_checkpoint_port(port.clone())
            .with_stream_observer(observer)
            .with_stale_tool_result_masking(KEPT_TOOL_ROUNDS, [super::knowledge::NAME.to_string()])
            // A helper's answer stays whole until a request would not fit;
            // many delegations must not overflow a small model's context.
            .with_tool_results_kept_until_tight([super::delegate::NAME.to_string()]);
        if let Some(tool) = host_knowledge_tool {
            behavior = behavior.with_host_knowledge_tool(tool);
        }
        if let Some(tool) = host_delegate_tool {
            behavior = behavior.with_host_tool(super::delegate::NAME, tool);
        }
        if let Some(hooks) = hooks {
            behavior = behavior.with_hook_registry(hooks);
        }
        let behavior: Box<dyn AgentBehavior> = Box::new(behavior);
        Ok(PreparedActivation {
            controller: self.clone(),
            activation,
            config,
            input,
            behavior: Some(behavior),
            control,
            port,
            output,
            settled: false,
            _execution: execution,
        })
    }

    pub(super) fn admit_provider(
        &self,
        activation: &ActivationRef,
        request_digest: String,
        request_bytes: u64,
        bounds: ProviderExecutionBounds,
    ) -> Result<ProviderCallClaim> {
        let mut state = self.lock()?;
        state.execution_admission()?;
        let snapshot = state.current(activation)?;
        if snapshot.contract().blockers().iter().any(|item| {
            item.blocker.activation == *activation && item.state == TurnBlockerState::Pending
        }) {
            return Err(error(
                "provider dispatch is blocked by an exact pending human wait",
            ));
        }
        if state
            .content
            .activation_output_reservation(&snapshot, activation)
            .map_err(error)?
            .is_none()
        {
            return Err(error(
                "provider dispatch requires both activation settlement reservations",
            ));
        }
        state
            .memory
            .candidate_reservation(&state.canonical, activation)
            .map_err(error)?;
        let bound = state
            .bound
            .get(&activation.activation_id)
            .filter(|bound| bound.activation == *activation && !bound.control.is_cancelled())
            .cloned()
            .ok_or_else(|| error("provider activation has no current execution owner"))?;
        if let Some(reserve) = state.host_observation_shortfall(activation, 1) {
            return Err(error(super::repository_snapshot::reserve_message(reserve)));
        }
        let result = state.authority.claim_provider_call(
            &bound.lease,
            ProviderCallIntent {
                call_id: uuid::Uuid::new_v4().to_string(),
                provider: bound.profile.provider,
                model: bound.profile.model,
                request_sha256: request_digest,
                request_bytes,
                reservation: DispatchReservation {
                    tokens: bounds.token_limit,
                    cost_microunits: bounds.cost_microunits,
                },
                max_response_bytes: bounds.response_bytes as u64,
            },
            now_ms()?,
        );
        // These are definitive pre-dispatch refusals: no claim was persisted.
        // A failed or uncertain write must still fence every caller.
        use axocoatl_session::control_authority::AuthorityError;
        match result {
            Err(
                failure @ (AuthorityError::Denied
                | AuthorityError::StaleLease
                | AuthorityError::Capacity
                | AuthorityError::Invalid(_)),
            ) => Err(error(failure)),
            other => state.fail_closed(other.map_err(error)),
        }
    }

    pub(super) fn settle_provider(
        &self,
        claim: &ProviderCallClaim,
        outcome: &ProviderCallOutcome,
    ) -> Result<()> {
        let mut state = self.lock()?;
        let result = state
            .authority
            .settle_provider_call(claim, outcome)
            .map_err(error);
        state.fail_closed(result)
    }

    pub(super) fn provider_boundary_failed(&self, reason: String) {
        if let Ok(mut state) = self.lock() {
            let _ = state.fail_closed::<()>(Err(error(reason)));
        }
    }
}

struct CheckpointPort {
    controller: SessionDispatchController,
    reservation: ReservedActivationCheckpoint,
    candidate: Mutex<Option<CheckpointRef>>,
}

#[async_trait]
impl ActivationCheckpointPort for CheckpointPort {
    async fn restore(&self) -> std::result::Result<Option<AgentCheckpoint>, String> {
        let state = self.controller.lock().map_err(|e| e.to_string())?;
        state.ready().map_err(|e| e.to_string())?;
        state
            .current(self.reservation.activation())
            .map_err(|e| e.to_string())?;
        let mut checkpoint = state
            .memory
            .starting_checkpoint_for(&self.reservation)
            .map_err(|e| e.to_string())?;
        let usage = state
            .conversation_usage(self.reservation.conversation_id())
            .map_err(|e| e.to_string())?;
        if let Some(checkpoint) = &mut checkpoint {
            checkpoint.cumulative_token_usage = usage.usage;
            checkpoint.cumulative_token_usage_known = usage.complete;
        } else if usage.usage.total() > 0 || !usage.complete {
            // An explicitly empty transcript can still carry incurred usage
            // from failed generations. It is not a missing checkpoint fallback.
            checkpoint = Some(AgentCheckpoint {
                version: 0,
                agent_id: self.reservation.conversation_id().as_str().into(),
                checkpoint_time: 0,
                session_messages: vec![],
                cumulative_token_usage: usage.usage,
                cumulative_token_usage_known: usage.complete,
                behavior_state: None,
            });
        }
        Ok(checkpoint)
    }

    async fn stage(&self, checkpoint: &AgentCheckpoint) -> std::result::Result<(), String> {
        let mut state = self.controller.lock().map_err(|e| e.to_string())?;
        // Late diagnostic state may survive Stop/closure. It cannot accept it.
        let result = (|| {
            let usage = state.conversation_usage(self.reservation.conversation_id())?;
            let actual = AgentCheckpoint {
                version: checkpoint.version,
                agent_id: checkpoint.agent_id.clone(),
                checkpoint_time: checkpoint.checkpoint_time,
                session_messages: checkpoint.session_messages.clone(),
                cumulative_token_usage: usage.usage,
                cumulative_token_usage_known: usage.complete,
                behavior_state: checkpoint.behavior_state.clone(),
            };
            let reference = state
                .memory
                .stage_reserved_candidate(&self.reservation, &actual)
                .map_err(error)?;
            let mut candidate = self
                .candidate
                .lock()
                .map_err(|_| error("candidate receipt lock failed"))?;
            if candidate.as_ref().is_some_and(|old| old != &reference) {
                return Err(error("actor attempted to replace its immutable candidate"));
            }
            *candidate = Some(reference);
            Ok(())
        })();
        state.fail_closed(result).map_err(|e| e.to_string())
    }

    fn maximum_checkpoint_bytes(&self) -> usize {
        self.reservation.max_checkpoint_bytes()
    }
}

impl PreparedActivation {
    #[cfg(test)]
    pub(crate) fn execution_boundary_for_test(&self) -> Arc<dyn ToolExecutionBoundary> {
        self.control.execution_boundary().unwrap().clone()
    }

    pub fn activation(&self) -> &ActivationRef {
        &self.activation
    }

    /// Uses the native behavior's existing provider/tool/compaction loop. No
    /// controller lock is held across any of these actor awaits.
    pub async fn run(mut self) -> Result<SettledActivation> {
        self.controller.lock()?.execution_admission()?;
        self.controller
            .capture_activation_repository(
                &self.activation,
                axocoatl_session::execution_content::RepositorySnapshotPhase::Before,
            )
            .await?;
        self.controller
            .validate_standing_candidate(&self.activation)?;
        let mut behavior = self
            .behavior
            .take()
            .ok_or_else(|| error("activation already consumed"))?;
        let mut outcome = match behavior.on_start(&self.config).await {
            Ok(()) => {
                behavior
                    .execute_controlled(self.input.clone(), self.control.clone())
                    .await
            }
            Err(error) => Err(error),
        };
        if let Err(error) = behavior.on_stop().await {
            outcome = Err(error);
        }
        self.controller
            .capture_activation_repository(
                &self.activation,
                axocoatl_session::execution_content::RepositorySnapshotPhase::After,
            )
            .await?;
        let scope_violation = self.controller.write_scope_violation(&self.activation)?;
        let (mut text, mut completed, mut failure) = match outcome {
            Ok(AgentRunOutcome::Completed(output)) => (output.content, true, None),
            Ok(AgentRunOutcome::Cancelled { partial_output, .. }) => {
                (partial_output.content, false, Some("Stopped".into()))
            }
            Err(error) => (
                format!("Activation failed: {error}"),
                false,
                Some(error.to_string()),
            ),
        };
        if let Some(violation) = scope_violation {
            text = format!("Activation failed: {violation}.\n\n{text}");
            completed = false;
            failure = Some(violation);
        }
        let mut state = self.controller.lock()?;
        let usage = state
            .authority
            .provider_usage(&self.activation)
            .map_err(error)?
            .tokens;
        let checkpoint = self
            .port
            .candidate
            .lock()
            .map_err(|_| error("candidate receipt lock failed"))?
            .clone();
        let eligible = completed
            && !self.control.is_cancelled()
            && state.poisoned.is_none()
            && checkpoint.is_some()
            && state.current(&self.activation).is_ok();
        let result = (|| {
            // Close the shared admission gate before publishing a terminal
            // record. Completion can never leave an old provider lease live.
            let revision = state.authority.revision().map_err(error)?;
            state
                .authority
                .stop_activation(&self.activation, revision)
                .map_err(error)?;
            let output = state
                .content
                .settle_activation_output(
                    &self.output,
                    ActivationOutputContent {
                        activation: self.activation.clone(),
                        recorded_at_unix_ms: now_ms()?,
                        text,
                        usage: if usage.complete {
                            ExecutionUsage::Measured { usage: usage.usage }
                        } else {
                            ExecutionUsage::Unknown {
                                known_subtotal: usage.usage,
                            }
                        },
                        kind: if eligible {
                            OutputKind::Final
                        } else {
                            OutputKind::Partial
                        },
                    },
                )
                .map_err(error)?;
            let accepted = eligible && output.complete_output().is_some();
            if accepted {
                state.append(
                    &format!("accept:{}", self.activation.activation_id.as_str()),
                    TurnContractEvent::AcceptActivation {
                        activation: self.activation.clone(),
                        checkpoint: Box::new(checkpoint.clone().unwrap()),
                        output: output.reference().clone(),
                    },
                )?;
            } else {
                failure.get_or_insert_with(|| {
                    if output.content().is_truncated() {
                        "Output exceeded its reserved capacity".into()
                    } else {
                        "Activation did not complete with current durable evidence".into()
                    }
                });
                if state.current(&self.activation).is_ok() {
                    state.append(
                        &format!("fail:{}", self.activation.activation_id.as_str()),
                        TurnContractEvent::FailActivation {
                            activation: self.activation.clone(),
                            evidence: output.reference().clone(),
                        },
                    )?;
                }
            }
            Ok(SettledActivation {
                activation: self.activation.clone(),
                output,
                checkpoint,
                accepted,
                failure,
            })
        })();
        let result = state.fail_closed(result).and_then(|settled| {
            state.reconcile_control_commands()?;
            Ok(settled)
        });
        state.changed.notify_waiters();
        self.settled = result.is_ok();
        drop(state);
        result
    }
}

impl Drop for PreparedActivation {
    fn drop(&mut self) {
        if !self.settled {
            // Process loss is recovered by the canonical epoch interruption.
            // In-process abandonment can already close its exact live gate.
            let _ = self.controller.stop_activation(&self.activation);
            self.control.cancel();
            if let Ok(mut state) = self.controller.lock() {
                if let Ok(snapshot) = state.current(&self.activation) {
                    if let Some(intent) = snapshot.contract().stop_requested() {
                        // A dropped prepared/cancelled actor cannot later run.
                        // Its stream/checkpoint/provider evidence stays partial
                        // or unknown; this does not invent a completed output.
                        let result = state.append(
                            &format!(
                                "turn-stop-abandoned:{}",
                                self.activation.activation_id.as_str()
                            ),
                            TurnContractEvent::FailActivation {
                                activation: self.activation.clone(),
                                evidence: intent.evidence.clone(),
                            },
                        );
                        let _ = state.fail_closed(result);
                    }
                }
            }
        }
    }
}

impl DispatchState {
    fn conversation_usage(&self, conversation: &NodeConversationId) -> Result<MeasuredTokenUsage> {
        self.conversation_usage_excluding(conversation, None)
    }

    fn conversation_usage_excluding(
        &self,
        conversation: &NodeConversationId,
        unbound: Option<&ActivationRef>,
    ) -> Result<MeasuredTokenUsage> {
        // Prepared generations can become Superseded without ever starting.
        // The immutable journal, rather than a missing authority record or the
        // latest state label, establishes whether dispatch was ever possible.
        let started = self
            .canonical
            .records()
            .map_err(error)?
            .iter()
            .filter_map(|record| match &record.event {
                TurnContractEvent::StartActivation { input } => Some(&input.activation),
                TurnContractEvent::StartPreparedActivation { activation } => Some(activation),
                _ => None,
            })
            .map(|activation| ((&activation.turn_id, &activation.activation_id), activation))
            .collect::<HashMap<_, _>>();
        let mut total = match self
            .memory
            .legacy_baseline_checkpoint(conversation)
            .map_err(error)?
        {
            Some(baseline) => MeasuredTokenUsage {
                usage: baseline.cumulative_token_usage,
                complete: baseline.cumulative_token_usage_known,
            },
            None => MeasuredTokenUsage::known(TokenUsageStats::default()),
        };
        let mut turns = HashSet::new();
        for record in self.canonical.records().map_err(error)? {
            if !turns.insert(record.turn_id.clone()) {
                continue;
            }
            let snapshot = self.canonical.snapshot(&record.turn_id).map_err(error)?;
            let activations = snapshot
                .contract()
                .activations()
                .iter()
                .filter(|item| {
                    &item.input.conversation_id == conversation
                        && started
                            .get(&(&item.activation.turn_id, &item.activation.activation_id))
                            .is_some_and(|started| *started == &item.activation)
                        && unbound != Some(&item.activation)
                })
                .map(|item| item.activation.clone())
                .collect::<Vec<_>>();
            if activations.is_empty() {
                continue;
            }
            if record.turn_id == self.turn_id {
                for activation in &activations {
                    merge_usage(
                        &mut total,
                        self.authority
                            .provider_usage(activation)
                            .map_err(error)?
                            .tokens,
                    )?;
                }
            } else {
                if !snapshot
                    .contract()
                    .state()
                    .is_some_and(LogicalTurnState::is_closed)
                {
                    return Err(error(
                        "historical provider accounting still has a live turn",
                    ));
                }
                let namespace = self
                    .canonical
                    .component_namespace(ExecutionComponent::ControlAuthority {
                        turn_id: record.turn_id.clone(),
                    })
                    .map_err(error)?;
                let usage = ControlAuthority::read_provider_usage_owned(namespace, &activations)
                    .map_err(error)?;
                merge_usage(&mut total, usage.tokens)?;
            }
        }
        Ok(total)
    }
}

fn merge_usage(total: &mut MeasuredTokenUsage, next: MeasuredTokenUsage) -> Result<()> {
    total.usage.input_tokens = total
        .usage
        .input_tokens
        .checked_add(next.usage.input_tokens)
        .ok_or_else(|| error("incurred input usage overflow"))?;
    total.usage.output_tokens = total
        .usage
        .output_tokens
        .checked_add(next.usage.output_tokens)
        .ok_or_else(|| error("incurred output usage overflow"))?;
    total.usage.reasoning_tokens = match (total.usage.reasoning_tokens, next.usage.reasoning_tokens)
    {
        (None, None) => None,
        (a, b) => Some(
            a.unwrap_or(0)
                .checked_add(b.unwrap_or(0))
                .ok_or_else(|| error("incurred reasoning usage overflow"))?,
        ),
    };
    total.complete &= next.complete;
    Ok(())
}
