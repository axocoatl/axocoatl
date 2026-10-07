//! The Session controller's port for external agents (Claude Code, Codex).
//! Included into `session_dispatch` as `external_port` (one module line in
//! `session_dispatch.rs`), so it reads the controller's retained state the
//! way the native factory and the repository tools do. Owner: workstream
//! `agents`.
//!
//! - [`SessionDispatchController::external_activation_factory`] wraps the
//!   native factory: an activation whose retained definition names an
//!   external runtime gets an [`ExternalProgramProvider`]; every other one
//!   goes to the native factory unchanged.
//! - The actor's tool loop then runs exactly as for a native writer: the
//!   activation is admitted against its grant, the host captures the checkout
//!   before it, the one model call is admitted and reserved by the Session
//!   provider, the host captures the checkout after it and judges its write
//!   scope, and the output is settled and accepted or failed.
//! - That one model call is the program run
//!   ([`SessionDispatchController::run_external_program`]): one supervised
//!   process in the Session container, as the writer user (the container's
//!   hardening applies to every supervised process), with the egress
//!   credential of this activation, on the repository's execution lease,
//!   while the call's reservation is open.

use super::*;
use crate::external_agent::{self as external, ExternalActivationResult, ExternalItem};
use axocoatl_config::loadout::AgentRuntime;
use axocoatl_core::{AgentConfig, MeasuredTokenUsage, TokenUsageStats};
use axocoatl_exec::protocol::{ExecRequest, ProcessOutcome, ServerMessage, StdinDescriptor};
use axocoatl_isolation::egress::{GrantKind, GrantSpec, ProcessEnv};
use axocoatl_isolation::supervisor_transport::{RunningSupervisedCommand, SupervisedExecution};
use axocoatl_isolation::ExecIdentity;
use axocoatl_llm::{
    ChatRequest, ChatResponse, FinishReason, LlmProvider, ProviderCapabilities, ProviderError,
    ProviderExecutionBounds, StreamEvent,
};
use axocoatl_session::network_record::{Decision as RecordDecision, NetworkEvent};
use axocoatl_token::TokenCounter;
use std::pin::Pin;
use std::time::Duration;
use tokio_stream::Stream;

/// The most bytes of one model call's streamed response (the Session
/// provider's own ceiling).
const RESPONSE_BYTES: usize = 1024 * 1024;
/// stderr kept from a run.
const STDERR_BYTES: usize = 64 * 1024;
/// How often a running program's route requests are counted.
pub(crate) const DEFAULT_METER_INTERVAL: Duration = Duration::from_secs(1);

/// Network record events, by sequence number, for counting a run's route
/// requests. The Session's own record in production; a test may supply
/// another source.
pub(crate) trait RouteRequestSource: Send + Sync {
    fn events_after(
        &self,
        after: Option<u64>,
        limit: usize,
    ) -> std::result::Result<Vec<(u64, NetworkEvent)>, String>;
}

/// Counts the allowed route requests made on connections opened with one
/// egress credential (its token tag), reading the record incrementally.
pub(crate) struct RouteMeter {
    source: Arc<dyn RouteRequestSource>,
    token: String,
    after: Option<u64>,
    connections: HashSet<String>,
    allowed: u64,
}

impl RouteMeter {
    pub(crate) fn new(source: Arc<dyn RouteRequestSource>, token: impl Into<String>) -> Self {
        Self {
            source,
            token: token.into(),
            after: None,
            connections: HashSet::new(),
            allowed: 0,
        }
    }

    /// Allowed route requests so far.
    pub(crate) fn poll(&mut self) -> std::result::Result<u64, String> {
        loop {
            let events = self.source.events_after(self.after, 1000)?;
            if events.is_empty() {
                return Ok(self.allowed);
            }
            for (seq, event) in events {
                self.after = Some(self.after.map_or(seq, |after| after.max(seq)));
                match event {
                    NetworkEvent::Open {
                        conn,
                        token: Some(token),
                        decision: RecordDecision::Allow,
                        ..
                    } if token == self.token => {
                        self.connections.insert(conn);
                    }
                    NetworkEvent::Request {
                        conn,
                        decision: RecordDecision::Allow,
                        ..
                    } if self.connections.contains(&conn) => {
                        self.allowed = self.allowed.saturating_add(1);
                    }
                    _ => {}
                }
            }
        }
    }
}

/// The Session's own network record.
struct RecordSource {
    controller: SessionDispatchController,
}

impl RouteRequestSource for RecordSource {
    fn events_after(
        &self,
        after: Option<u64>,
        limit: usize,
    ) -> std::result::Result<Vec<(u64, NetworkEvent)>, String> {
        Ok(self
            .controller
            .read_network_record(after, limit)
            .map_err(|error| error.to_string())?
            .map(|(lines, _)| {
                lines
                    .into_iter()
                    .map(|line| (line.seq, line.event))
                    .collect()
            })
            .unwrap_or_default())
    }
}

/// A test's change to the argv a run uses.
#[cfg(test)]
pub(crate) type AdjustArgv = Arc<dyn Fn(Vec<String>) -> Vec<String> + Send + Sync>;

/// How external runs are observed.
#[derive(Clone)]
pub(crate) struct ExternalSettings {
    /// Where route requests are counted; `None` reads the Session's record.
    pub(crate) source: Option<Arc<dyn RouteRequestSource>>,
    pub(crate) meter_interval: Duration,
    /// Tests point the pinned programs at a local upstream's port.
    #[cfg(test)]
    pub(crate) adjust_argv: Option<AdjustArgv>,
}

impl Default for ExternalSettings {
    fn default() -> Self {
        Self {
            source: None,
            meter_interval: DEFAULT_METER_INTERVAL,
            #[cfg(test)]
            adjust_argv: None,
        }
    }
}

/// One program run: the argv through the supervisor and the prompt on stdin.
pub(crate) struct ExternalProgram {
    pub(crate) argv: Vec<String>,
    pub(crate) stdin: Vec<u8>,
    /// A tighter wall clock than the activation's grant expiry, if any.
    pub(crate) timeout_ms: Option<u64>,
}

/// A parsed external run and how it ended.
pub(crate) struct ExternalOutcome {
    pub(crate) result: ExternalActivationResult,
    pub(crate) route_requests: u64,
    pub(crate) stopped: Option<String>,
    pub(crate) stderr: String,
    /// What the run's model call reserved.
    pub(crate) reserved: ProviderExecutionBounds,
}

/// What a run returned.
#[derive(Debug)]
pub(crate) struct ExternalRun {
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: String,
    pub(crate) outcome: ProcessOutcome,
    /// Allowed route requests made with the run's credential.
    pub(crate) route_requests: u64,
    /// Why the host stopped the program, when it did.
    pub(crate) stopped: Option<String>,
}

impl ExternalRun {
    pub(crate) fn exit_code(&self) -> Option<i32> {
        match self.outcome {
            ProcessOutcome::Exited { code } => Some(code),
            _ => None,
        }
    }
}

/// An external definition resolved for one activation.
struct ResolvedExternal {
    config: AgentConfig,
    profile: ExecutionProfile,
    runtime: AgentRuntime,
}

impl DispatchState {
    /// The external runtime of `input`'s retained definition, validated the
    /// way the native factory validates a native one; `None` for a definition
    /// that is not external.
    fn resolve_external(
        &self,
        input: &ActivationInputManifest,
    ) -> Result<Option<ResolvedExternal>> {
        // Whether the definition is external at all. Anything this cannot
        // read is left to the native factory, which refuses it in its own
        // words.
        let Ok(snapshot) = self
            .execution_admission()
            .and_then(|()| self.current(&input.activation))
        else {
            return Ok(None);
        };
        let Ok(resolved) = self.content.validate_input(&snapshot, input) else {
            return Ok(None);
        };
        let ActivationEvidenceContent::Definition {
            definition_id,
            profile,
            configuration,
            ..
        } = resolved.definition
        else {
            return Ok(None);
        };
        let Ok(config) = serde_json::from_str::<AgentConfig>(&configuration) else {
            return Ok(None);
        };
        if external::runtime_for_provider(&config.provider).is_none() {
            return Ok(None);
        }
        let runtime = external::validate_external_config(&config).map_err(error)?;
        if config.id.0 != input.conversation_id.as_str()
            || profile.definition != definition_id.as_str()
            || profile.provider != config.provider
            || profile.model != config.model
            || profile.tools != config.tools
            || profile.write_scope != config.writes
            || profile.isolation != "in-process"
            || serde_json::to_string(&config).map_err(error)? != configuration
        {
            return Err(error(
                "external definition differs from the admitted Agent configuration",
            ));
        }
        if matches!(input.repository, RepositoryInput::Unavailable) {
            return Err(error(
                "an external agent runs in the Session's checkout, and this activation has none",
            ));
        }
        let grant = input
            .grant
            .as_ref()
            .ok_or_else(|| error("external activation lacks an exact grant"))?;
        let current = self
            .authority
            .grant_policy(grant.grant_id.as_str())
            .map_err(error)?;
        if !self.captured_grant_is_current(resolved.grant.as_ref(), &resolved.budget, &current)? {
            return Err(error(
                "external activation budget or grant differs from current authority",
            ));
        }
        self.authority
            .validate_activation_grant(
                &input.activation,
                grant.grant_id.as_str(),
                &profile,
                now_ms()?,
            )
            .map_err(error)?;
        Ok(Some(ResolvedExternal {
            config,
            profile,
            runtime,
        }))
    }
}

/// External definitions run their program; every other one goes to the
/// native factory.
struct ExternalActivationFactory {
    controller: SessionDispatchController,
    native: Arc<dyn AutonomousActivationFactory>,
    counter: Arc<dyn TokenCounter>,
    settings: ExternalSettings,
}

impl SessionDispatchController {
    /// The activation factory of a native Session controller with external
    /// agents: see the module documentation.
    pub(crate) fn external_activation_factory(
        &self,
        native: Arc<dyn AutonomousActivationFactory>,
        counter: Arc<dyn TokenCounter>,
        settings: ExternalSettings,
    ) -> Arc<dyn AutonomousActivationFactory> {
        Arc::new(ExternalActivationFactory {
            controller: self.clone(),
            native,
            counter,
            settings,
        })
    }
}

#[async_trait]
impl AutonomousActivationFactory for ExternalActivationFactory {
    async fn resources(
        &self,
        input: &ActivationInputManifest,
    ) -> std::result::Result<AutonomousActivationResources, String> {
        let resolved = {
            let state = self.controller.lock().map_err(|error| error.to_string())?;
            state
                .resolve_external(input)
                .map_err(|error| error.to_string())?
        };
        let Some(resolved) = resolved else {
            return self.native.resources(input).await;
        };
        let provider = Arc::new(ExternalProgramProvider {
            controller: self.controller.clone(),
            activation: input.activation.clone(),
            runtime: resolved.runtime,
            provider: resolved.config.provider.clone(),
            model: resolved.config.model.clone(),
            settings: self.settings.clone(),
        });
        Ok(AutonomousActivationResources {
            config: resolved.config,
            profile: resolved.profile,
            provider,
            counter: self.counter.clone(),
            tools: Arc::new(axocoatl_tools::ToolExecutor::new()),
        })
    }
}

/// The "model" of an external activation: its one call runs the program.
pub(crate) struct ExternalProgramProvider {
    controller: SessionDispatchController,
    activation: ActivationRef,
    runtime: AgentRuntime,
    provider: String,
    model: String,
    settings: ExternalSettings,
}

type EventStream =
    Pin<Box<dyn Stream<Item = std::result::Result<StreamEvent, ProviderError>> + Send>>;

impl ExternalProgramProvider {
    fn failure(&self, message: impl Into<String>) -> ProviderError {
        ProviderError::Stream(message.into())
    }
}

/// The host's final-answer note: the grant cannot pay for another round.
fn host_wrap_up(request: &ChatRequest) -> Option<String> {
    let text = request.messages.last()?.text_content()?;
    text.starts_with("[Note from the host:")
        .then(|| text.to_string())
}

/// The events of one finished run: the work log as reasoning, then the
/// usage the program reported (unless it overran the reservation), then
/// the answer, or the reason there is none. A failed run that reported its
/// usage settles to that usage; one that did not keeps the reservation.
pub(crate) fn run_events(
    runtime: AgentRuntime,
    model: &str,
    result: &ExternalActivationResult,
    route_requests: u64,
    stopped: Option<&str>,
    reserved: &ProviderExecutionBounds,
    stderr: &str,
) -> Vec<std::result::Result<StreamEvent, ProviderError>> {
    let mut events: Vec<std::result::Result<StreamEvent, ProviderError>> =
        external::work_log(runtime, model, result, route_requests, stopped)
            .into_iter()
            .map(|delta| Ok(StreamEvent::ReasoningDelta { delta }))
            .collect();
    let provider = external::runtime_provider(runtime)
        .unwrap_or("external")
        .to_string();
    let usage = result.usage();
    let overrun = usage.and_then(|(input, output, cost)| {
        let tokens = input.saturating_add(output);
        if tokens > reserved.token_limit {
            Some(format!(
                "the program reported {tokens} tokens, more than the {} its grant still allowed; \
                 the whole reservation stays charged",
                reserved.token_limit
            ))
        } else if cost.is_some_and(|cost| cost > reserved.cost_microunits) {
            Some(format!(
                "the program reported a cost above the {} micro-dollars its grant still allowed; \
                 the whole reservation stays charged",
                reserved.cost_microunits
            ))
        } else {
            None
        }
    });
    let reported = overrun.is_none() && result.usage_complete;
    if let (Some((input, output, cost)), true) = (usage, overrun.is_none()) {
        events.push(Ok(StreamEvent::UsageObservation(MeasuredTokenUsage {
            usage: TokenUsageStats::new(
                usize::try_from(input).unwrap_or(usize::MAX),
                usize::try_from(output).unwrap_or(usize::MAX),
            ),
            complete: result.usage_complete,
        })));
        if let Some(cost) = cost {
            events.push(Ok(StreamEvent::CostObservation {
                cost_microunits: cost,
            }));
        }
    }
    let failure = if let Some(reason) = stopped {
        Some(format!("Axocoatl stopped the program: {reason}"))
    } else if let Some(overrun) = overrun {
        Some(overrun)
    } else if !result.succeeded() {
        let mut reason = match (result.exit_code, result.final_answer.is_some()) {
            (Some(code), true) => format!("the program exited with status {code}"),
            (Some(code), false) => {
                format!("the program exited with status {code} and gave no answer")
            }
            (None, _) => "the program did not exit normally".to_string(),
        };
        if let Some(error) = result.last_error() {
            reason.push_str(": ");
            reason.push_str(&external::bound_text(error, 2048));
        } else if !stderr.trim().is_empty() {
            let tail: String = stderr
                .chars()
                .rev()
                .take(1024)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            reason.push_str(": ");
            reason.push_str(tail.trim());
        }
        Some(reason)
    } else {
        None
    };
    match failure {
        Some(message) if reported => {
            events.push(Err(ProviderError::RefusedResponse { provider, message }))
        }
        Some(message) => events.push(Err(ProviderError::Stream(message))),
        None => {
            events.push(Ok(StreamEvent::TextDelta {
                delta: result.final_answer.clone().unwrap_or_default(),
            }));
            events.push(Ok(StreamEvent::Done {
                finish_reason: FinishReason::Stop,
            }));
        }
    }
    events
}

#[async_trait]
impl LlmProvider for ExternalProgramProvider {
    fn provider_id(&self) -> &str {
        &self.provider
    }
    fn model_id(&self) -> &str {
        &self.model
    }
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            streaming: true,
            ..Default::default()
        }
    }
    fn model_constraints_known(&self, _request: &ChatRequest) -> bool {
        false
    }
    /// The run reserves everything the grant still allows: its tokens and
    /// its spending.
    fn execution_bounds(&self, _request: &ChatRequest) -> Option<ProviderExecutionBounds> {
        let allowance = self.controller.agent_allowance(&self.activation)?;
        Some(ProviderExecutionBounds {
            token_limit: allowance.tokens.unwrap_or(0).max(1),
            cost_microunits: allowance.cost_microunits.unwrap_or(0),
            response_bytes: RESPONSE_BYTES,
        })
    }
    /// There is no follow-up call: the run is the activation's only one.
    fn follow_up_execution_bounds(
        &self,
        _request: &ChatRequest,
        _added_prompt_tokens: u64,
    ) -> Option<ProviderExecutionBounds> {
        Some(ProviderExecutionBounds {
            token_limit: 0,
            cost_microunits: 0,
            response_bytes: 1,
        })
    }
    async fn chat(
        &self,
        _request: ChatRequest,
    ) -> std::result::Result<ChatResponse, ProviderError> {
        Err(ProviderError::InvalidRequest {
            provider: self.provider.clone(),
            message: "an external agent runs only through its streamed activation".into(),
        })
    }
    async fn chat_stream(
        &self,
        request: ChatRequest,
    ) -> std::result::Result<EventStream, ProviderError> {
        if let Some(note) = host_wrap_up(&request) {
            return Err(ProviderError::BudgetExhausted {
                provider: self.provider.clone(),
                message: format!(
                    "the grant cannot pay for the external program run: {}",
                    external::bound_text(&note, 512)
                ),
            });
        }
        let outcome = self
            .controller
            .run_external_request(
                &self.activation,
                self.runtime,
                &self.model,
                external::prompt_text(&request.messages),
                None,
                &self.settings,
            )
            .await
            .map_err(|error| self.failure(error.to_string()))?;
        let events = run_events(
            self.runtime,
            &self.model,
            &outcome.result,
            outcome.route_requests,
            outcome.stopped.as_deref(),
            &outcome.reserved,
            &outcome.stderr,
        );
        Ok(Box::pin(tokio_stream::iter(events)))
    }
}

/// Requests Stop of the owned process when the waiting caller goes away
/// before it ends; the spawned task still joins it and settles the lease.
struct StopOnDrop {
    cancellation: axocoatl_isolation::supervisor_transport::SupervisorCancellation,
    armed: bool,
}

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        if self.armed {
            self.cancellation.cancel();
        }
    }
}

impl SessionDispatchController {
    /// Run `runtime` with `prompt` for `activation` and parse its output:
    /// the external activation's program run, for its provider and for
    /// `AxocoatlDaemon::run_external_activation`. The definition bound to the
    /// activation must name exactly this runtime and model, and the
    /// activation's admitted model call must be open: its reservation (what
    /// the grant still allowed) is what the run may spend.
    pub(crate) async fn run_external_request(
        &self,
        activation: &ActivationRef,
        runtime: AgentRuntime,
        model: &str,
        prompt: String,
        timeout_ms: Option<u64>,
        settings: &ExternalSettings,
    ) -> Result<ExternalOutcome> {
        let reserved = {
            let state = self.lock()?;
            let bound = state
                .bound
                .get(&activation.activation_id)
                .filter(|bound| bound.activation == *activation)
                .ok_or_else(|| error("external activation has no current execution owner"))?;
            if external::runtime_for_provider(&bound.profile.provider) != Some(runtime)
                || bound.profile.model != model
            {
                return Err(error(
                    "the external program differs from the activation's admitted definition",
                ));
            }
            // The run's one admitted model call holds the reservation: what
            // the grant allowed when the call was admitted.
            if state
                .authority
                .provider_usage(activation)
                .map_err(error)?
                .unsettled_calls
                == 0
            {
                return Err(error(
                    "an external program runs only inside its activation's admitted model call",
                ));
            }
            let reservation = state
                .authority
                .largest_provider_reservation(activation)
                .map_err(error)?
                .ok_or_else(|| error("the activation's model call has no reservation"))?;
            ProviderExecutionBounds {
                token_limit: reservation.tokens,
                cost_microunits: reservation.cost_microunits,
                response_bytes: RESPONSE_BYTES,
            }
        };
        if prompt.len() > external::MAX_PROMPT_BYTES {
            return Err(error(format!(
                "the external agent's prompt is {} bytes, more than {}",
                prompt.len(),
                external::MAX_PROMPT_BYTES
            )));
        }
        let mut argv = external::command_argv(runtime, model).map_err(error)?;
        if runtime == AgentRuntime::ClaudeCode && reserved.cost_microunits > 0 {
            // The program's own spending stop, at what the grant still allows.
            argv.extend(external::claude_code::budget_args(reserved.cost_microunits));
        }
        #[cfg(test)]
        if let Some(adjust) = &settings.adjust_argv {
            argv = adjust(argv);
        }
        let run = self
            .run_external_program(
                activation,
                ExternalProgram {
                    argv,
                    stdin: prompt.into_bytes(),
                    timeout_ms,
                },
                settings,
            )
            .await?;
        let mut result = external::parse_output(runtime, &run.stdout).map_err(error)?;
        result.exit_code = run.exit_code();
        if let Some(reason) = &run.stopped {
            result.items.push(ExternalItem::Error {
                message: format!("Axocoatl stopped the program: {reason}"),
            });
        }
        Ok(ExternalOutcome {
            result,
            route_requests: run.route_requests,
            stopped: run.stopped,
            stderr: run.stderr,
            reserved,
        })
    }

    /// Run one external program for `activation`: a supervised process in
    /// the Session container as the writer user, with this activation's
    /// egress credential, while its admitted model call is open. Counts the
    /// credential's allowed route requests and stops the program when they
    /// reach the invocations the grant still allows, at the activation's
    /// wall clock (its grant's expiry), or when the activation is stopped.
    pub(crate) async fn run_external_program(
        &self,
        activation: &ActivationRef,
        program: ExternalProgram,
        settings: &ExternalSettings,
    ) -> Result<ExternalRun> {
        let (owner, reference, control, agent, expires_at_ms, route_allowance) = {
            let state = self.lock()?;
            state.execution_admission()?;
            state.current(activation)?;
            let bound = state
                .bound
                .get(&activation.activation_id)
                .filter(|bound| bound.activation == *activation && !bound.control.is_cancelled())
                .cloned()
                .ok_or_else(|| error("external activation has no current execution owner"))?;
            if external::runtime_for_provider(&bound.profile.provider).is_none() {
                return Err(error("only an external definition runs a program"));
            }
            let usage = state.authority.provider_usage(activation).map_err(error)?;
            if usage.unsettled_calls == 0 {
                return Err(error(
                    "an external program runs only inside its activation's admitted model call",
                ));
            }
            let reference = bound
                .repository
                .as_ref()
                .ok_or_else(|| error("external activation has no repository"))?
                .reference()
                .clone();
            let owner = state
                .repository_owners
                .get(&reference)
                .cloned()
                .ok_or_else(|| error("external activation's repository has no live owner"))?;
            repository::validate_retained_repository(&state, &owner, &reference)?;
            owner.validate_dispatch_resource().map_err(error)?;
            let grant = state
                .authority
                .grant_status(bound.grant.grant_id.as_str())
                .map_err(error)?;
            let allowance = state
                .agent_allowance(activation)
                .and_then(|allowance| allowance.invocations)
                .unwrap_or(0);
            (
                owner,
                reference,
                bound.control.clone(),
                bound.profile.definition.clone(),
                grant.policy.expires_at_ms,
                allowance,
            )
        };
        if route_allowance == 0 {
            return Err(error(
                "the grant allows no more invocations, so the program could make no model request",
            ));
        }
        let mut lease = tokio::select! {
            lease = owner.queued_execution_lease() => lease.map_err(error)?,
            _ = control.cancelled() => return Err(error("the activation stopped before its program started")),
        };
        let now = now_ms()?;
        if expires_at_ms <= now {
            return Err(error(
                "the activation's grant expired before its program started",
            ));
        }
        let timeout_ms = (expires_at_ms - now)
            .min(program.timeout_ms.unwrap_or(u64::MAX))
            .clamp(1, axocoatl_exec::protocol::MAX_TIMEOUT_MS);
        let request = ExecRequest {
            protocol: axocoatl_exec::protocol::PROTOCOL_VERSION,
            invocation_id: format!("external:{}", activation.activation_id.as_str()),
            argv: program.argv,
            timeout_ms,
            stdout_bytes: external::MAX_READ_BACK_BYTES + 4096,
            stderr_bytes: STDERR_BYTES,
            stdin: Some(StdinDescriptor::for_bytes(&program.stdin).map_err(error)?),
            write_restriction: None,
        };
        request.validate().map_err(error)?;
        // External programs talk to their model only through the Session's
        // routes; without the decision point they would have no way out but
        // an unrecorded one, so they do not run.
        let authority = lease.sandbox().egress_authority().ok_or_else(|| {
            error("an external agent runs only in a Session under network: egress")
        })?;
        let mut spec = GrantSpec::new(GrantKind::Agent);
        spec.activation_id = Some(activation.activation_id.as_str().to_string());
        spec.node_id = Some(activation.node_id.as_str().to_string());
        spec.agent = Some(agent);
        spec.process = Some(request.invocation_id.clone());
        let grant = authority.grant(spec).await.map_err(error)?;
        let token = grant.token_tag.clone();
        let command = lease
            .sandbox()
            .prepare_supervised_command_as(
                request.clone(),
                Some(program.stdin),
                ProcessEnv {
                    env_file: grant.env_file.as_deref(),
                },
                ExecIdentity::Writer,
            )
            .await
            .map_err(error)?;
        if command.request() != &request {
            return Err(error("supervisor changed the external program's request"));
        }
        lease.bind_supervised_command(&command).map_err(error)?;
        let cancellation = command.cancellation();
        let (running, ticket) = {
            let state = self.lock()?;
            state.execution_admission()?;
            state.current(activation)?;
            repository::validate_retained_repository(&state, &owner, &reference)?;
            let ticket = state.acquire_execution_ticket(self)?;
            lease.mark_dispatched().map_err(error)?;
            let running = command.dispatch().map_err(error)?;
            (running, ticket)
        };
        // No await between dispatch and handing the process to its owner.
        let controller = self.clone();
        let task = tokio::spawn(async move {
            let _ticket = ticket;
            // The credential ends when its process is settled.
            let _grant = grant;
            finish_external(controller, lease, running).await
        });
        let mut task = task;
        let mut guard = StopOnDrop {
            cancellation: cancellation.clone(),
            armed: true,
        };
        let source = settings.source.clone().unwrap_or_else(|| {
            Arc::new(RecordSource {
                controller: self.clone(),
            })
        });
        let mut meter = RouteMeter::new(source, token);
        let mut stopped = None;
        let mut ticker = tokio::time::interval(settings.meter_interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let execution = loop {
            tokio::select! {
                joined = &mut task => {
                    guard.armed = false;
                    break joined.map_err(|failure| {
                        error(format!("the external program's owner task was lost: {failure}"))
                    })??;
                }
                _ = ticker.tick() => {
                    match meter.poll() {
                        Ok(requests) if requests > route_allowance && stopped.is_none() => {
                            stopped = Some(format!(
                                "it made {requests} model requests and its grant allowed {route_allowance}"
                            ));
                            cancellation.cancel();
                        }
                        Ok(_) => {}
                        Err(failure) if stopped.is_none() => {
                            // Unmetered requests cannot run on.
                            stopped = Some(format!("its model requests could not be counted: {failure}"));
                            cancellation.cancel();
                        }
                        Err(_) => {}
                    }
                }
                _ = control.cancelled(), if stopped.is_none() => {
                    stopped = Some("the activation was stopped".into());
                    cancellation.cancel();
                }
            }
        };
        let route_requests = meter.poll().unwrap_or(meter.allowed);
        let ServerMessage::Finished {
            outcome,
            stdout,
            stderr,
            ..
        } = execution.result()
        else {
            return Err(error("the external program has no terminal observation"));
        };
        execution
            .result()
            .validate_for(execution.request())
            .map_err(error)?;
        let stdout = stdout
            .retained_bytes(execution.request().stdout_bytes)
            .map_err(error)?;
        let stderr = String::from_utf8_lossy(
            &stderr
                .retained_bytes(execution.request().stderr_bytes)
                .map_err(error)?,
        )
        .into_owned();
        if matches!(outcome, ProcessOutcome::TimedOut) && stopped.is_none() {
            stopped = Some("it reached the activation's time limit".into());
        }
        if let ProcessOutcome::LaunchFailed { message } | ProcessOutcome::Failed { message } =
            outcome
        {
            return Err(error(format!(
                "the external program could not run: {message}"
            )));
        }
        Ok(ExternalRun {
            stdout,
            stderr,
            outcome: outcome.clone(),
            route_requests,
            stopped,
        })
    }
}

/// Join the supervised process (Stop is requested when the activation is
/// cancelled) and settle the repository lease with its exact proof.
async fn finish_external(
    controller: SessionDispatchController,
    lease: crate::bootstrap::session_repository::SessionRepositoryExecutionLease,
    running: RunningSupervisedCommand,
) -> Result<SupervisedExecution> {
    let execution = running.finish().await;
    let settlement = execution
        .as_ref()
        .ok()
        .and_then(SupervisedExecution::settlement);
    let released = match settlement {
        Some(proof) => lease.settle_supervised(proof).map_err(error),
        None => {
            drop(lease);
            Err(error(
                "external program supervision did not prove process settlement",
            ))
        }
    };
    if let Err(failure) = &released {
        if let Ok(mut state) = controller.lock() {
            let _ = state.fail_closed::<()>(Err(error(failure)));
        }
    }
    released?;
    execution.map_err(error)
}

#[cfg(test)]
impl SessionDispatchController {
    /// The text and reasoning deltas recorded in `activation`'s stream.
    pub(crate) fn activation_stream_for_test(&self, activation: &ActivationRef) -> Vec<String> {
        use axocoatl_session::execution_content::ActivationStreamPayload;
        let state = self.lock().unwrap();
        let snapshot = state.canonical.snapshot(&activation.turn_id).unwrap();
        state
            .content
            .activation_stream(&snapshot, activation)
            .unwrap()
            .into_iter()
            .filter_map(|view| match view.content.payload {
                ActivationStreamPayload::Text { delta }
                | ActivationStreamPayload::ReasoningSummary { delta } => Some(delta),
                _ => None,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Events(Mutex<Vec<NetworkEvent>>);

    impl RouteRequestSource for Events {
        fn events_after(
            &self,
            after: Option<u64>,
            limit: usize,
        ) -> std::result::Result<Vec<(u64, NetworkEvent)>, String> {
            Ok(self
                .0
                .lock()
                .unwrap()
                .iter()
                .enumerate()
                .map(|(index, event)| (index as u64 + 1, event.clone()))
                .filter(|(seq, _)| after.is_none_or(|after| *seq > after))
                .take(limit)
                .collect())
        }
    }

    fn open(conn: &str, token: &str) -> NetworkEvent {
        serde_json::from_value(serde_json::json!({
            "kind": "open", "conn": conn, "decision": "allow", "host": "api.anthropic.com",
            "port": 443, "conn_kind": "connect", "token": token
        }))
        .unwrap()
    }

    fn request(conn: &str, decision: &str) -> NetworkEvent {
        serde_json::from_value(serde_json::json!({
            "kind": "request", "conn": conn, "seq_in_conn": 1, "method": "POST",
            "path": "/v1/messages", "host": "api.anthropic.com", "decision": decision
        }))
        .unwrap()
    }

    #[test]
    fn the_meter_counts_allowed_route_requests_of_one_credential_only() {
        let events = Arc::new(Events(Mutex::new(vec![
            open("g1:1", "mine"),
            open("g1:2", "other"),
            request("g1:1", "allow"),
            request("g1:2", "allow"),
            request("g1:1", "deny"),
        ])));
        let mut meter = RouteMeter::new(events.clone(), "mine");
        assert_eq!(meter.poll().unwrap(), 1);
        events.0.lock().unwrap().push(request("g1:1", "allow"));
        events.0.lock().unwrap().push(open("g1:3", "mine"));
        events.0.lock().unwrap().push(request("g1:3", "allow"));
        assert_eq!(meter.poll().unwrap(), 3);
        assert_eq!(meter.poll().unwrap(), 3);
    }

    fn bounds(tokens: u64, cost: u64) -> ProviderExecutionBounds {
        ProviderExecutionBounds {
            token_limit: tokens,
            cost_microunits: cost,
            response_bytes: RESPONSE_BYTES,
        }
    }

    fn claude(
        answer: Option<&str>,
        usage: Option<(u64, u64, Option<u64>)>,
        exit: i32,
    ) -> ExternalActivationResult {
        let mut items = vec![ExternalItem::ToolCall {
            name: "Bash".into(),
            arguments: "ls".into(),
        }];
        if let Some(answer) = answer {
            items.push(ExternalItem::AssistantText {
                text: answer.into(),
            });
        }
        if let Some((input_tokens, output_tokens, cost_microunits)) = usage {
            items.push(ExternalItem::Usage {
                input_tokens,
                output_tokens,
                cost_microunits,
            });
        }
        ExternalActivationResult {
            final_answer: answer.map(str::to_string),
            items,
            exit_code: Some(exit),
            usage_complete: usage.is_some(),
        }
    }

    fn kinds(events: &[std::result::Result<StreamEvent, ProviderError>]) -> Vec<String> {
        events
            .iter()
            .map(|event| match event {
                Ok(StreamEvent::ReasoningDelta { .. }) => "log".into(),
                Ok(StreamEvent::TextDelta { delta }) => format!("text:{delta}"),
                Ok(StreamEvent::UsageObservation(usage)) => format!(
                    "usage:{}+{}:{}",
                    usage.usage.input_tokens, usage.usage.output_tokens, usage.complete
                ),
                Ok(StreamEvent::CostObservation { cost_microunits }) => {
                    format!("cost:{cost_microunits}")
                }
                Ok(StreamEvent::Done { .. }) => "done".into(),
                Ok(_) => "other".into(),
                Err(ProviderError::RefusedResponse { message, .. }) => format!("refused:{message}"),
                Err(error) => format!("error:{error}"),
            })
            .filter(|kind| kind != "log")
            .collect()
    }

    #[test]
    fn a_finished_run_settles_to_its_report_and_answers_with_its_final_text() {
        let events = run_events(
            AgentRuntime::ClaudeCode,
            "m",
            &claude(Some("Done."), Some((100, 20, Some(5000))), 0),
            2,
            None,
            &bounds(1000, 10_000),
            "",
        );
        assert_eq!(
            kinds(&events),
            ["usage:100+20:true", "cost:5000", "text:Done.", "done"]
        );
        assert!(
            matches!(&events[0], Ok(StreamEvent::ReasoningDelta { delta }) if delta.starts_with("[external agent]"))
        );
    }

    #[test]
    fn failures_settle_to_reported_usage_or_keep_the_reservation() {
        // Exited with an error after reporting usage: refused, settles to it.
        let events = run_events(
            AgentRuntime::ClaudeCode,
            "m",
            &claude(None, Some((10, 0, None)), 1),
            1,
            None,
            &bounds(1000, 0),
            "",
        );
        let seen = kinds(&events);
        assert_eq!(seen[0], "usage:10+0:true");
        assert!(seen[1].starts_with("refused:the program exited with status 1 and gave no answer"));
        // No usage report: the reservation stays charged.
        let events = run_events(
            AgentRuntime::Codex,
            "m",
            &claude(Some("half"), None, 137),
            1,
            None,
            &bounds(1000, 0),
            "killed by something\n",
        );
        let seen = kinds(&events);
        assert_eq!(seen.len(), 1);
        assert!(
            seen[0].starts_with("error:Streaming error: the program exited with status 137"),
            "{seen:?}"
        );
        // Stopped by the host: no answer, even with one printed.
        let events = run_events(
            AgentRuntime::ClaudeCode,
            "m",
            &claude(Some("answer"), Some((10, 1, None)), 0),
            5,
            Some("it made 5 model requests and its grant allowed 4"),
            &bounds(1000, 0),
            "",
        );
        assert!(kinds(&events)
            .last()
            .unwrap()
            .contains("Axocoatl stopped the program: it made 5"));
        // An overrun is never reported as settled usage.
        let events = run_events(
            AgentRuntime::ClaudeCode,
            "m",
            &claude(Some("answer"), Some((900, 200, Some(1))), 0),
            1,
            None,
            &bounds(1000, 10),
            "",
        );
        let seen = kinds(&events);
        assert_eq!(seen.len(), 1, "{seen:?}");
        assert!(seen[0].contains("more than the 1000 its grant still allowed"));
    }

    #[test]
    fn the_hosts_final_answer_note_is_recognized() {
        let mut request = ChatRequest::simple("task");
        assert!(host_wrap_up(&request).is_none());
        request.messages.push(axocoatl_core::ChatMessage::user(
            "[Note from the host: the Session budget allows only 1 more model or tool call(s), so tools are no longer available.]",
        ));
        assert!(host_wrap_up(&request).is_some());
    }
}
