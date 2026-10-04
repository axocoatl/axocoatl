//! Explicit, owned tool and autonomous actor adapter for schema-2 Sessions.
//!
//! This is not a live bootstrap/migration path or a complete scheduler. The host
//! authenticates requests, prepares real runtime resources, and supplies actors
//! matching the admitted immutable profile. The one-shot autonomous port reserves
//! candidate/output storage and requires provider-enforced whole-call bounds.
//! The older tool-only binding does not establish provider accounting coverage;
//! nonzero tool reservations still require a supporting executor and are refused.
//! A stored reference alone cannot establish runtime availability.

#[path = "session_dispatch_human_context.rs"]
pub(crate) mod human_context;
pub use human_context::HumanControlContext;

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use axocoatl_actor::{
    AdmittedToolInvocation, AgentRunControl, ToolExecutionBoundary, ToolInvocationOutcome,
    ToolInvocationRequest,
};
use axocoatl_memory::activation_state::ActivationStateStore;
use axocoatl_session::control_authority::{
    ActivationLease, AuthorityGrant, ControlAuthority, DispatchReservation, ExecutionProfile,
    GrantLimits,
};
use axocoatl_session::control_command::ControlCommandStore;
use axocoatl_session::execution_content::{
    ActivationEvidenceContent, ContentResolution, DurableToolArguments, ExecutionContentStore,
};
use axocoatl_session::execution_namespace::ExecutionComponent;
use axocoatl_session::execution_store::{
    DurableTurnReceipt, DurableTurnSnapshot, SessionExecutionStore,
};
use axocoatl_session::invocation_audit::{
    InvocationAudit, InvocationAuthority, InvocationEvidenceCommand, InvocationFinalEvidence,
    InvocationIntent, InvocationIntentCommand, InvocationOutcomeSource, InvocationReplayPolicy,
    ProviderReplayIdentity,
};
use axocoatl_session::turn_contract::*;
use sha2::{Digest, Sha256};

const MAX_RESULT_BYTES: usize = 1024 * 1024;

#[path = "session_dispatch_turn_stop.rs"]
mod turn_stop;
pub use turn_stop::TurnStopReceipt;

#[path = "session_dispatch_human_wait.rs"]
mod human_wait;

#[path = "session_dispatch_grant_review.rs"]
mod grant_review;
pub(crate) use grant_review::{retained_grant_decision, retained_grant_view};
pub use grant_review::{
    SessionGrantChange, SessionGrantDecision, SessionGrantPreview, SessionGrantView,
};
#[path = "session_dispatch_commands.rs"]
mod commands;
#[path = "session_dispatch_conditions.rs"]
mod conditions;
#[path = "session_dispatch_coordinator.rs"]
mod coordinator;
#[path = "session_dispatch_delegate.rs"]
mod delegate;
pub(crate) use delegate::changing_tools;
#[path = "session_dispatch_driver.rs"]
mod driver;
#[path = "session_dispatch_host_tools.rs"]
mod host_tools;
#[path = "session_dispatch_knowledge.rs"]
mod knowledge;
pub(crate) use coordinator::{NativeCoordinatorWorker, COORDINATOR_CHILD, DELEGATE_CHILD};
pub(crate) use host_tools::{
    host_tool_refusal, is_host_invocation_tool, HostInvocationContext, HostInvocationTool,
    HostToolDefinition,
};
#[path = "session_dispatch_graph.rs"]
mod graph;
pub(crate) use graph::pending_human_graph_receipt;
#[path = "session_dispatch_execution_lifetime.rs"]
mod execution_lifetime;
#[path = "session_dispatch_retained.rs"]
mod retained;
#[path = "session_dispatch_steering.rs"]
mod steering;
pub(crate) use retained::RetainedSessionStores;
#[path = "session_dispatch_history.rs"]
mod history;
#[path = "session_dispatch_host_controls.rs"]
mod host_controls;
#[path = "session_dispatch_input.rs"]
mod input;
#[path = "session_dispatch_lifecycle.rs"]
mod lifecycle;
#[path = "session_dispatch_provider.rs"]
mod provider;
#[path = "session_dispatch_repository.rs"]
mod repository;
#[path = "session_dispatch_repository_activation.rs"]
mod repository_activation;
#[path = "session_dispatch_repository_snapshot.rs"]
mod repository_snapshot;
#[path = "session_dispatch_run.rs"]
mod run;
#[path = "session_dispatch_stream.rs"]
mod stream;
#[path = "session_dispatch_turn_checks.rs"]
mod turn_checks;
#[path = "session_dispatch_turn_review.rs"]
mod turn_review;
pub use conditions::SettledRepositoryCheck;
pub use driver::{
    AutonomousActivationFactory, AutonomousNodeInput, AutonomousTurnDriver, TurnDriveOutcome,
};
pub use host_controls::{
    HumanBlockerResponse, HumanCheckChoice, HumanContinuationChoice, HumanContinuationSelection,
    HumanControlAction, HumanControlActionRequest, HumanPartialFinishSelection, HumanTurnControls,
};
pub use lifecycle::{FinalizedTurn, SuccessorTurn};
pub use repository::OwnedRepositoryCheck;
pub(crate) use repository_activation::validate_repository_tools;
pub use repository_activation::RepositoryActivationResource;
pub use run::{AutonomousActivationResources, PreparedActivation, SettledActivation};
#[path = "session_dispatch_native.rs"]
mod native;
pub(crate) use native::{
    CapturedNativeDefinition, NativeDefinitionPreparation, NativeProviderCredentials,
};
#[path = "session_dispatch_native_turn.rs"]
mod native_turn;
#[path = "session_dispatch_ways.rs"]
mod ways;

#[derive(Debug, thiserror::Error)]
#[error("Session dispatch: {0}")]
pub struct SessionDispatchError(String);
type Result<T> = std::result::Result<T, SessionDispatchError>;
fn error(value: impl std::fmt::Display) -> SessionDispatchError {
    SessionDispatchError(value.to_string())
}

#[derive(Clone)]
struct BoundActivation {
    activation: ActivationRef,
    actor_id: String,
    profile: ExecutionProfile,
    lease: ActivationLease,
    grant: GrantSnapshotRef,
    control: AgentRunControl,
    steering_open: bool,
    repository: Option<RepositoryActivationResource>,
}

struct DispatchState {
    canonical: SessionExecutionStore,
    turn_id: LogicalTurnId,
    content: ExecutionContentStore,
    memory: ActivationStateStore,
    audit: InvocationAudit,
    authority: ControlAuthority,
    commands: ControlCommandStore,
    driver: Option<String>,
    hooks: Option<Arc<axocoatl_tools::HookRegistry>>,
    knowledge: Option<knowledge::SharedKnowledge>,
    /// Host tools this controller's activations may list, by name.
    host_tools: host_tools::HostTools,
    human_waits: HashMap<BlockerId, human_wait::LiveHumanWait>,
    stream_bus: Option<crate::stream::StreamBus>,
    execution_admission_closed: bool,
    execution_lifetimes: Arc<execution_lifetime::ExecutionLifetimes>,
    changed: Arc<tokio::sync::Notify>,
    bound: HashMap<ActivationId, BoundActivation>,
    repository_checks:
        HashMap<ConditionRunId, axocoatl_isolation::supervisor_transport::SupervisorCancellation>,
    repository_owners:
        HashMap<EvidenceRef, crate::bootstrap::session_repository::SessionRepositoryOwner>,
    repository_reattachments: HashMap<EvidenceRef, EvidenceRef>,
    repository_registration:
        Option<std::sync::Weak<crate::bootstrap::session_dispatch::RepositoryRegistrationGate>>,
    poisoned: Option<String>,
    #[cfg(test)]
    fail_at: Option<TestFailure>,
}

/// One owner serializes canonical checks and durable admissions. Locks are never
/// held while waiting on a provider, tool, or actor cancellation settlement.
#[derive(Clone)]
pub struct SessionDispatchController {
    state: Arc<Mutex<DispatchState>>,
}

impl SessionDispatchController {
    pub fn open(canonical: SessionExecutionStore, turn_id: LogicalTurnId) -> Result<Self> {
        let snapshot = canonical.snapshot(&turn_id).map_err(error)?;
        if snapshot.request_ref().is_none() {
            return Err(error(
                "tool dispatch requires a canonical retained request binding",
            ));
        }
        let content = ExecutionContentStore::open_owned(
            canonical
                .component_namespace(ExecutionComponent::ExecutionContent)
                .map_err(error)?,
        )
        .map_err(error)?;
        if !matches!(
            content.project(&snapshot).map_err(error)?.request,
            ContentResolution::Available { .. }
        ) {
            return Err(error("canonical request content is unavailable"));
        }
        let memory = ActivationStateStore::open_owned(
            canonical
                .component_namespace(ExecutionComponent::ActivationState)
                .map_err(error)?,
        )
        .map_err(error)?;
        Self::open_retained(
            RetainedSessionStores {
                canonical,
                content,
                memory,
            },
            turn_id,
        )
        .map_err(|failure| failure.error)
    }

    fn lock(&self) -> Result<execution_lifetime::DispatchGuard<'_>> {
        let state = self
            .state
            .lock()
            .map_err(|_| error("controller lock failed; reconstruct before work"))?;
        Ok(execution_lifetime::DispatchGuard::new(self, state))
    }

    pub fn snapshot(&self) -> Result<DurableTurnSnapshot> {
        let state = self.lock()?;
        state.canonical.snapshot(&state.turn_id).map_err(error)
    }

    /// A durable Running row alone cannot advertise a live Stop after restart.
    /// Execution tickets belong to actual process-local driver/child work.
    pub(crate) fn live_owned_turn(&self) -> Result<Option<LogicalTurnId>> {
        let state = self.lock()?;
        state.ready()?;
        if state.execution_lifetimes.is_idle() {
            return Ok(None);
        }
        let snapshot = state.canonical.snapshot(&state.turn_id).map_err(error)?;
        Ok(
            (snapshot.contract().state() == Some(LogicalTurnState::Running))
                .then(|| state.turn_id.clone()),
        )
    }

    /// Closed canonical state can precede the final process-local ticket drop.
    /// The host must leave resource release to that owner until it is idle.
    pub(crate) fn has_owned_execution(&self) -> Result<bool> {
        let state = self.lock()?;
        state.ready()?;
        Ok(state.driver.is_some()
            || !state.repository_checks.is_empty()
            || !state.execution_lifetimes.is_idle()
            // Isolated Ways keep their existing attempt owner until the
            // checked Keep/NoKeep cleanup, even after candidates settle.
            || state.is_isolated_ways()?)
    }

    /// Construct graph and retained evidence under the same canonical lock.
    /// Read access itself never mints command authority or restarts execution.
    pub fn control_plane(&self) -> Result<crate::session_control_plane::SessionTurnControlPlane> {
        let state = self.lock()?;
        let snapshot = state.canonical.snapshot(&state.turn_id).map_err(error)?;
        state.project_control_plane(&snapshot, true)
    }

    /// Trusted host setup, not an RPC authorization surface. A supplied graph
    /// change remains subject to canonical validation; closures close dispatch
    /// first. This adapter does not implement command-source permission policy.
    pub fn append_host_event(&self, envelope: TurnContractEnvelope) -> Result<DurableTurnReceipt> {
        let mut state = self.lock()?;
        state.ready()?;
        if envelope.turn_id != state.turn_id
            || envelope.session_id != state.canonical.owner().session_id
        {
            return Err(error("event belongs to another Session or turn"));
        }
        if matches!(
            envelope.event,
            TurnContractEvent::RequestTurnStop { .. }
                | TurnContractEvent::RequestPartialFinish { .. }
                | TurnContractEvent::Begin { .. }
                | TurnContractEvent::ReviseGraph { .. }
                | TurnContractEvent::OpenBlocker { .. }
                | TurnContractEvent::ResolveBlocker { .. }
                | TurnContractEvent::AbandonBlocker { .. }
                | TurnContractEvent::AcceptActivation { .. }
                | TurnContractEvent::RecordIntent { .. }
                | TurnContractEvent::RecordOutcome { .. }
                | TurnContractEvent::ProveNotDispatched { .. }
                | TurnContractEvent::RecordConditionIntent { .. }
                | TurnContractEvent::ResolveConditionIntent { .. }
        ) {
            return Err(error(
                "request, acceptance and invocation events require their owning storage protocol",
            ));
        }
        if matches!(envelope.event, TurnContractEvent::Close { .. }) {
            state.close_and_promote(envelope.clone())?;
            return state.canonical.append(envelope).map_err(error);
        }
        let mut preview = state
            .canonical
            .snapshot(&state.turn_id)
            .map_err(error)?
            .contract()
            .clone();
        preview.apply(&envelope).map_err(error)?;
        let result = state.canonical.append(envelope).map_err(error);
        if result.is_err() {
            for bound in state.bound.values() {
                bound.control.cancel();
            }
        }
        let receipt = state.fail_closed(result)?;
        state.reconcile_control_commands()?;
        state.changed.notify_waiters();
        Ok(receipt)
    }

    /// Retain bounded immutable setup evidence. Role validation happens again
    /// against the exact canonical input manifest before actor admission.
    pub fn retain_activation_evidence(
        &self,
        evidence: ActivationEvidenceContent,
    ) -> Result<EvidenceRef> {
        let mut state = self.lock()?;
        state.ready()?;
        let result = state
            .content
            .retain_activation_evidence(evidence)
            .map(|receipt| receipt.reference().clone())
            .map_err(error);
        state.fail_closed(result)
    }

    /// Called only after the host authenticates the user grant. Its topology is
    /// checked against the declared graph, and its exact bytes must also be the
    /// retained grant snapshot selected by each activation input.
    pub fn install_grant(&self, grant: AuthorityGrant) -> Result<()> {
        let mut state = self.lock()?;
        state.ready()?;
        let snapshot = state.canonical.snapshot(&state.turn_id).map_err(error)?;
        let graph = snapshot
            .contract()
            .graph()
            .ok_or_else(|| error("missing graph"))?;
        if !graph.nodes.iter().any(|node| node.node_id == grant.holder) {
            return Err(error("grant holder is outside the declared graph"));
        }
        let mut descendants = HashSet::new();
        let mut frontier = vec![grant.holder.clone()];
        while let Some(parent) = frontier.pop() {
            for edge in graph
                .dependencies
                .iter()
                .filter(|edge| edge.parent == parent)
            {
                if descendants.insert(edge.child.clone()) {
                    frontier.push(edge.child.clone());
                }
            }
        }
        if grant
            .descendants
            .iter()
            .any(|node| !descendants.contains(node))
        {
            return Err(error("grant includes a non-descendant"));
        }
        let revision = state.authority.revision().map_err(error)?;
        let result = state
            .authority
            .install_grant(grant, revision)
            .map_err(error);
        state.fail_closed(result)
    }

    /// Bind an exact current generation after physically resolving its immutable
    /// inputs. The returned control must be passed to a supported actor behavior.
    /// This adapter's boundary deliberately refuses to invent child topology or
    /// authority; helpers are admitted only through the `delegate` port.
    pub fn bind_activation(
        &self,
        activation: ActivationRef,
        profile: ExecutionProfile,
        configuration: String,
        control: AgentRunControl,
        reservation: DispatchReservation,
    ) -> Result<AgentRunControl> {
        if control.execution_boundary().is_some() {
            return Err(error("control already carries another execution boundary"));
        }
        if reservation.tokens != 0 || reservation.cost_microunits != 0 {
            return Err(error(
                "nonzero tool reservations require executor enforcement",
            ));
        }
        let mut state = self.lock()?;
        state.execution_admission()?;
        if state.bound.contains_key(&activation.activation_id) {
            return Err(error(
                "activation is already bound; never reuse its actor execution",
            ));
        }
        let result = state.bind(activation.clone(), profile, &configuration, control.clone());
        let bound = state.fail_closed(result)?;
        state.bound.insert(activation.activation_id.clone(), bound);
        Ok(
            control.with_execution_boundary(Arc::new(ActivationBoundary {
                controller: self.clone(),
                activation,
            })),
        )
    }

    /// Acknowledges closed dispatch authority, then requests cooperative Stop.
    /// It does not claim rollback or settlement of already claimed invocations.
    pub fn stop_activation(&self, activation: &ActivationRef) -> Result<()> {
        let mut state = self.lock()?;
        state.ready()?;
        let bound = state
            .bound
            .get(&activation.activation_id)
            .filter(|bound| &bound.activation == activation)
            .cloned()
            .ok_or_else(|| error("activation has no exact live owner"))?;
        let revision = state.authority.revision().map_err(error)?;
        let result = state
            .authority
            .stop_activation(activation, revision)
            .map_err(error);
        let result = state.fail_closed(result);
        bound.control.cancel();
        result
    }
}

struct ActivationBoundary {
    controller: SessionDispatchController,
    activation: ActivationRef,
}
struct InvocationAdmission {
    controller: SessionDispatchController,
    arguments: DurableToolArguments,
    intent: InvocationIntent,
    authority_ref: EvidenceRef,
    repository: Option<repository_activation::RepositoryInvocation>,
    /// The bound host tool for a `web_search`, `web_fetch`, `browser` or
    /// `browser_check` call.
    host_executor: Option<Arc<axocoatl_tools::ToolExecutor>>,
    _execution: execution_lifetime::ExecutionTicket,
}

#[async_trait]
impl ToolExecutionBoundary for ActivationBoundary {
    fn approval_actor_scope(&self) -> std::result::Result<String, String> {
        let state = self.controller.lock().map_err(|error| error.to_string())?;
        let snapshot = state
            .current(&self.activation)
            .map_err(|error| error.to_string())?;
        let item = snapshot
            .contract()
            .activations()
            .iter()
            .find(|item| item.activation == self.activation)
            .ok_or_else(|| "approval activation is unavailable".to_string())?;
        Ok(format!(
            "{}:{}",
            self.activation.session_id.as_str(),
            item.conversation_id.as_str()
        ))
    }

    async fn request_human_approval(
        &self,
        request: &ToolInvocationRequest,
        display: serde_json::Value,
        timeout: std::time::Duration,
    ) -> std::result::Result<axocoatl_tools::HookApprovalResolution, String> {
        self.controller
            .wait_for_human_approval(&self.activation, request, display, timeout)
            .await
            .map_err(|error| error.to_string())
    }

    fn take_guidance(
        &self,
        final_boundary: bool,
    ) -> std::result::Result<Option<axocoatl_actor::SteeringDelivery>, String> {
        self.controller
            .take_activation_guidance(&self.activation, final_boundary)
            .map_err(|error| error.to_string())
    }

    fn preadmission_refusal(
        &self,
        request: &ToolInvocationRequest,
        earlier: u32,
    ) -> Option<String> {
        self.controller
            .host_observation_reserve_refusal(&self.activation, request, earlier)
    }

    async fn admit(
        &self,
        request: &ToolInvocationRequest,
    ) -> std::result::Result<Box<dyn AdmittedToolInvocation>, String> {
        self.controller
            .admit_invocation(&self.activation, request)
            .map(|admission| Box::new(admission) as Box<dyn AdmittedToolInvocation>)
            .map_err(|error| error.to_string())
    }

    async fn admit_or_decline(
        &self,
        request: &ToolInvocationRequest,
    ) -> std::result::Result<axocoatl_actor::execution_boundary::InvocationAdmission, String> {
        use axocoatl_actor::execution_boundary::InvocationAdmission as Outcome;
        match self
            .controller
            .admit_or_decline_invocation(&self.activation, request)
        {
            Ok(Ok(admission)) => Ok(Outcome::Admitted(Box::new(admission))),
            Ok(Err(reason)) => Ok(Outcome::Declined(reason)),
            Err(error) => Err(error.to_string()),
        }
    }
}

impl SessionDispatchController {
    fn admit_invocation(
        &self,
        activation: &ActivationRef,
        request: &ToolInvocationRequest,
    ) -> Result<InvocationAdmission> {
        self.admit_or_decline_invocation(activation, request)?
            .map_err(error)
    }

    /// Admission, or the reason an Agent's call is declined because it would
    /// spend invocations the host holds back. Checked under the same lock as
    /// the claim, so concurrent calls that each passed the early check cannot
    /// spend them together; a declined call records nothing.
    fn admit_or_decline_invocation(
        &self,
        activation: &ActivationRef,
        request: &ToolInvocationRequest,
    ) -> Result<std::result::Result<InvocationAdmission, String>> {
        let mut state = self.lock()?;
        state.execution_admission()?;
        let group = request.provider_response_group;
        if request.tool_call.name == axocoatl_session::control_authority::REPOSITORY_CAPTURE_PORT
            && (!repository_snapshot::is_host_observation(group)
                || repository_snapshot::is_digest_group(group))
        {
            return Err(error(
                "the repository capture port belongs to the host; an Agent cannot call it",
            ));
        }
        let agent_call = !repository_snapshot::is_host_observation(group)
            || repository_snapshot::is_digest_group(group);
        // A call the turn has no room for is declined before anything is
        // written, so reaching a per-turn bound never fences the controller.
        let room = if agent_call {
            state.tool_call_room()
        } else {
            state.turn_record_room()
        };
        if room == 0 {
            return Ok(Err(if agent_call {
                repository_snapshot::record_full_message()
            } else {
                repository_snapshot::RECORD_FULL.to_owned()
            }));
        }
        if agent_call {
            if let Some(reserve) =
                state.host_observation_shortfall(activation, repository_snapshot::TOOL_CALL_NEEDS)
            {
                return Ok(Err(repository_snapshot::reserve_message(reserve)));
            }
        }
        if let Some(reason) = state.host_tool_admission_refusal(activation, &request.tool_call.name)
        {
            return Ok(Err(reason));
        }
        let (arguments, intent, authority_ref) = state.admit(activation, request)?;
        let repository = repository_activation::RepositoryInvocation::for_admission(
            &state,
            self.clone(),
            &intent,
        );
        let repository = state.fail_closed(repository)?;
        let host_executor = state.host_invocation_executor(self, &intent);
        let host_executor = state.fail_closed(host_executor)?;
        Ok(Ok(InvocationAdmission {
            controller: self.clone(),
            arguments,
            intent,
            authority_ref,
            repository,
            host_executor,
            _execution: state.acquire_execution_ticket(self)?,
        }))
    }
}

#[async_trait]
impl AdmittedToolInvocation for InvocationAdmission {
    fn tool_executor(&self) -> Option<Arc<axocoatl_tools::ToolExecutor>> {
        self.repository
            .as_ref()
            .map(|repository| repository.executor())
            .or_else(|| self.host_executor.clone())
    }

    async fn record_outcome(
        self: Box<Self>,
        outcome: &ToolInvocationOutcome,
    ) -> std::result::Result<(), String> {
        let mut state = self.controller.lock().map_err(|error| error.to_string())?;
        // An unrelated admission failure or Stop cannot discard outcomes from
        // already claimed tools. Individual poisoned stores still fail closed.
        let uncertain = ToolInvocationOutcome::Unknown {
            reason: "repository process outcome requires reconciliation".into(),
        };
        let outcome = if self
            .repository
            .as_ref()
            .is_some_and(|repository| repository.is_uncertain())
        {
            &uncertain
        } else {
            outcome
        };
        let result = state.settle(&self.arguments, &self.intent, &self.authority_ref, outcome);
        state
            .fail_closed(result)
            .map_err(|error| error.to_string())?;
        Ok(())
    }
}

impl DispatchState {
    fn captured_grant_is_current(
        &self,
        captured: Option<&AuthorityGrant>,
        budget: &GrantLimits,
        policy: &AuthorityGrant,
    ) -> Result<bool> {
        let Some(captured) = captured else {
            return Ok(false);
        };
        Ok(captured.id == policy.id
            && captured.limits == *budget
            && self
                .authority
                .permits_captured_grant(captured)
                .map_err(error)?)
    }

    fn validate_physical_input(
        &self,
        input: &ActivationInputManifest,
        profile: &ExecutionProfile,
        configuration: &str,
        policy: &AuthorityGrant,
        repository: Option<&RepositoryActivationResource>,
    ) -> Result<()> {
        let snapshot = self.canonical.snapshot(&self.turn_id).map_err(error)?;
        let resolved = self
            .content
            .validate_input(&snapshot, input)
            .map_err(error)?;
        let ActivationEvidenceContent::Definition {
            profile: retained,
            configuration: retained_configuration,
            ..
        } = resolved.definition
        else {
            return Err(error("input definition has the wrong evidence role"));
        };
        let instance_matches = retained_configuration == configuration
            || (self.owns_instance_conversation(&input.activation.node_id)?
                && serde_json::to_string(&coordinator::instance_configuration(
                    &retained_configuration,
                    &input.conversation_id,
                )?)
                .map_err(error)?
                    == configuration);
        if retained != *profile
            || !instance_matches
            || !self.captured_grant_is_current(resolved.grant.as_ref(), &resolved.budget, policy)?
        {
            return Err(error(
                "physical definition, budget, or grant differs from current authority",
            ));
        }
        repository_activation::validate_input_resource(self, input, repository)?;
        Ok(())
    }

    fn reconcile(&mut self) -> Result<()> {
        // Canonical intent is written first. A crash before audit admission is
        // therefore retained as unknown, never upgraded to a no-effect proof.
        // This turn's invocations are checked, and every invocation of any
        // turn still without final evidence; settled invocations of closed
        // turns were checked while their turn was live and cannot change.
        let mut audit_intents: Vec<_> = self
            .audit
            .turn_invocations(&self.turn_id)
            .map_err(error)?
            .into_iter()
            .map(|audited| audited.intent)
            .collect();
        for audited in self.audit.unresolved().map_err(error)? {
            if audited.intent.activation.turn_id != self.turn_id {
                audit_intents.push(audited.intent);
            }
        }
        for intent in &audit_intents {
            let snapshot = self
                .canonical
                .snapshot(&intent.activation.turn_id)
                .map_err(error)?;
            let canonical = snapshot
                .contract()
                .invocations()
                .iter()
                .find(|item| item.invocation_id == intent.invocation_id)
                .ok_or_else(|| error("audit intent has no canonical predecessor"))?;
            if canonical.activation != intent.activation {
                return Err(error("audit intent differs from canonical activation"));
            }
            let arguments = self
                .content
                .tool_arguments(&snapshot, &intent.activation, &intent.invocation_id)
                .map_err(error)?
                .ok_or_else(|| error("audit arguments are missing"))?;
            self.content
                .read_tool_arguments(&arguments)
                .map_err(error)?;
            if arguments.protected_arguments() != &intent.arguments {
                return Err(error(
                    "audit intent differs from protected executable bytes",
                ));
            }
            let mut audit = self
                .audit
                .invocation(&intent.invocation_id)
                .map_err(error)?
                .unwrap()
                .clone();
            if audit.final_evidence.is_none() {
                if self
                    .content
                    .tool_result(&arguments)
                    .map_err(error)?
                    .is_none()
                {
                    self.reconcile_delegate_outcome(&snapshot, intent, &arguments)?;
                }
                if let Some(retained) = self.content.tool_result(&arguments).map_err(error)? {
                    // A returned status and exact protected payload reached durable
                    // storage before the audit write. Repair evidence only; this
                    // path cannot mint a lease or claim another dispatch.
                    self.content.read_tool_result(&retained).map_err(error)?;
                    let input = &snapshot
                        .contract()
                        .activations()
                        .iter()
                        .find(|item| item.activation == intent.activation)
                        .ok_or_else(|| error("retained outcome has no canonical input"))?
                        .input;
                    let grant = input
                        .grant
                        .as_ref()
                        .ok_or_else(|| error("retained outcome has no captured grant"))?;
                    let ActivationEvidenceContent::Grant { policy } = self
                        .content
                        .resolve_activation_evidence(&grant.evidence)
                        .map_err(error)?
                    else {
                        return Err(error("retained outcome grant has the wrong evidence role"));
                    };
                    let dispatch_policy = self
                        .authority
                        .recorded_grant_policy(
                            &intent.authority.grant_id,
                            intent.authority.grant_revision,
                        )
                        .map_err(error)?;
                    if grant.grant_id.as_str() != intent.authority.grant_id
                        || policy.id != dispatch_policy.id
                        || policy.profiles != dispatch_policy.profiles
                    {
                        return Err(error(
                            "retained outcome differs from immutable dispatch authority",
                        ));
                    }
                    self.audit
                        .record_evidence(InvocationEvidenceCommand {
                            command_id: CommandId::new(format!(
                                "recover-outcome:{}",
                                intent.invocation_id.as_str()
                            ))
                            .map_err(error)?,
                            expected_revision: 1,
                            invocation_id: intent.invocation_id.clone(),
                            activation: intent.activation.clone(),
                            authority: intent.authority.clone(),
                            evidence: InvocationFinalEvidence::Outcome {
                                outcome: retained.outcome(),
                                result: retained.protected_result().clone(),
                                redacted_preview: "[protected raw outcome]".into(),
                                source: InvocationOutcomeSource::Reconciliation,
                                authority_ref: grant.evidence.clone(),
                            },
                        })
                        .map_err(error)?;
                    audit = self
                        .audit
                        .invocation(&intent.invocation_id)
                        .map_err(error)?
                        .unwrap()
                        .clone();
                }
            }
            if let Some(InvocationFinalEvidence::Outcome {
                outcome, result, ..
            }) = &audit.final_evidence
            {
                let retained = self
                    .content
                    .tool_result(&arguments)
                    .map_err(error)?
                    .ok_or_else(|| error("audit outcome has no protected result"))?;
                if retained.protected_result() != result || retained.outcome() != *outcome {
                    return Err(error(
                        "audit outcome differs from protected result and returned status",
                    ));
                }
                // The content record binds status, full original digest/length,
                // and retained prefix. A prefix is never a complete result.
                self.content.read_tool_result(&retained).map_err(error)?;
            }
            match (&canonical.evidence, &audit.final_evidence) {
                (
                    InvocationEvidence::Outcome { outcome, evidence },
                    Some(InvocationFinalEvidence::Outcome {
                        outcome: actual,
                        result,
                        ..
                    }),
                ) if outcome == actual && evidence == &result.evidence_ref => {}
                (
                    InvocationEvidence::CancelledBeforeDispatch { evidence },
                    Some(InvocationFinalEvidence::NotDispatched {
                        evidence: actual, ..
                    }),
                ) if evidence == actual => {}
                (InvocationEvidence::Intent, _) => {}
                _ => return Err(error("canonical and audit outcomes disagree")),
            }
            if intent.activation.turn_id == self.turn_id {
                if let Some(final_evidence) = &audit.final_evidence {
                    let revision = self.authority.revision().map_err(error)?;
                    self.authority
                        .settle_dispatch(&intent.invocation_id, &self.audit, revision)
                        .map_err(error)?;
                    if matches!(canonical.evidence, InvocationEvidence::Intent)
                        && !snapshot
                            .contract()
                            .state()
                            .is_some_and(LogicalTurnState::is_closed)
                    {
                        let event = match final_evidence {
                            InvocationFinalEvidence::Outcome {
                                outcome, result, ..
                            } => TurnContractEvent::RecordOutcome {
                                invocation_id: intent.invocation_id.clone(),
                                outcome: *outcome,
                                evidence: result.evidence_ref.clone(),
                            },
                            InvocationFinalEvidence::NotDispatched { evidence, .. } => {
                                TurnContractEvent::ProveNotDispatched {
                                    invocation_id: intent.invocation_id.clone(),
                                    evidence: evidence.clone(),
                                }
                            }
                        };
                        self.append(
                            &format!("reconcile:{}", intent.invocation_id.as_str()),
                            event,
                        )?;
                    }
                }
            }
        }
        let snapshot = self.canonical.snapshot(&self.turn_id).map_err(error)?;
        for invocation in snapshot.contract().invocations() {
            let arguments = self
                .content
                .tool_arguments(&snapshot, &invocation.activation, &invocation.invocation_id)
                .map_err(error)?
                .ok_or_else(|| error("canonical invocation has no protected arguments"))?;
            self.content
                .read_tool_arguments(&arguments)
                .map_err(error)?;
            if self
                .audit
                .invocation(&invocation.invocation_id)
                .map_err(error)?
                .is_none()
                && !matches!(invocation.evidence, InvocationEvidence::Intent)
            {
                return Err(error("canonical outcome has no invocation audit"));
            }
        }
        Ok(())
    }

    fn ready(&self) -> Result<()> {
        if let Some(reason) = &self.poisoned {
            return Err(error(format!("recovery required: {reason}")));
        }
        Ok(())
    }
    fn fail_closed<T>(&mut self, result: Result<T>) -> Result<T> {
        if let Err(error) = &result {
            self.poisoned.get_or_insert_with(|| error.to_string());
            for bound in self.bound.values() {
                bound.control.cancel();
            }
            for check in self.repository_checks.values() {
                check.cancel();
            }
        }
        result
    }
    fn current(&self, activation: &ActivationRef) -> Result<DurableTurnSnapshot> {
        if activation.turn_id != self.turn_id {
            return Err(error("foreign turn"));
        }
        let snapshot = self.canonical.snapshot(&self.turn_id).map_err(error)?;
        let contract = snapshot.contract();
        if contract.state() != Some(LogicalTurnState::Running)
            || contract.epochs().last().is_none_or(|epoch| {
                epoch.id != activation.execution_epoch_id || epoch.state != EpochState::Running
            })
            || !contract.activations().iter().any(|item| {
                item.activation == *activation && item.state == ActivationState::Running
            })
        {
            return Err(error("activation is not current and running"));
        }
        Ok(snapshot)
    }
    fn bind(
        &mut self,
        activation: ActivationRef,
        profile: ExecutionProfile,
        configuration: &str,
        control: AgentRunControl,
    ) -> Result<BoundActivation> {
        self.bind_with_provider_gate(
            activation,
            profile,
            configuration,
            control,
            false,
            now_ms()?,
        )
    }

    fn bind_with_provider_gate(
        &mut self,
        activation: ActivationRef,
        profile: ExecutionProfile,
        configuration: &str,
        control: AgentRunControl,
        provider_gated: bool,
        admitted_at_ms: u64,
    ) -> Result<BoundActivation> {
        self.bind_with_repository(
            activation,
            profile,
            configuration,
            control,
            provider_gated,
            admitted_at_ms,
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn bind_with_repository(
        &mut self,
        activation: ActivationRef,
        profile: ExecutionProfile,
        configuration: &str,
        control: AgentRunControl,
        provider_gated: bool,
        admitted_at_ms: u64,
        repository: Option<RepositoryActivationResource>,
    ) -> Result<BoundActivation> {
        let snapshot = self.current(&activation)?;
        let input = &snapshot
            .contract()
            .activations()
            .iter()
            .find(|item| item.activation == activation)
            .unwrap()
            .input;
        let grant = input
            .grant
            .clone()
            .ok_or_else(|| error("activation has no exact grant"))?;
        let policy = self
            .authority
            .grant_policy(grant.grant_id.as_str())
            .map_err(error)?;
        if profile.definition != input.definition.definition_id.as_str() {
            return Err(error(
                "input grant or definition differs from current authority",
            ));
        }
        self.validate_physical_input(input, &profile, configuration, &policy, repository.as_ref())?;
        self.memory
            .record_input(&snapshot, &activation)
            .map_err(error)?;
        let revision = self.authority.revision().map_err(error)?;
        let register = if provider_gated {
            ControlAuthority::register_provider_activation
        } else {
            ControlAuthority::register_activation
        };
        let lease = register(
            &self.authority,
            activation.clone(),
            grant.grant_id.as_str(),
            profile.clone(),
            revision,
            admitted_at_ms,
        )
        .map_err(error)?;
        let grant = GrantSnapshotRef {
            grant_id: GrantId::new(&policy.id).map_err(error)?,
            revision: policy.revision,
            evidence: self
                .content
                .retain_activation_evidence(ActivationEvidenceContent::Grant { policy })
                .map_err(error)?
                .reference()
                .clone(),
        };
        Ok(BoundActivation {
            activation,
            actor_id: input.conversation_id.as_str().to_owned(),
            profile,
            lease,
            grant,
            control,
            steering_open: false,
            repository,
        })
    }
    fn append(&mut self, operation: &str, event: TurnContractEvent) -> Result<()> {
        let snapshot = self.canonical.snapshot(&self.turn_id).map_err(error)?;
        self.canonical
            .append(TurnContractEnvelope {
                schema_version: TURN_CONTRACT_SCHEMA_VERSION,
                command_id: CommandId::new(operation).map_err(error)?,
                expected_revision: snapshot.contract().revision(),
                session_id: self.canonical.owner().session_id.clone(),
                turn_id: self.turn_id.clone(),
                event,
            })
            .map_err(error)?;
        Ok(())
    }
    fn admit(
        &mut self,
        activation: &ActivationRef,
        request: &ToolInvocationRequest,
    ) -> Result<(DurableToolArguments, InvocationIntent, EvidenceRef)> {
        let snapshot = self.current(activation)?;
        let bound = self
            .bound
            .get(&activation.activation_id)
            .filter(|bound| bound.activation == *activation)
            .cloned()
            .ok_or_else(|| error("activation has no registered executor"))?;
        if bound.control.is_cancelled()
            || request.actor_id != bound.actor_id
            || request.provider_id != bound.profile.provider
            || request.model_id != bound.profile.model
            || request.provider_call_count == 0
            || request.provider_call_count > 128
            || request.provider_call_index >= request.provider_call_count
        {
            return Err(error(
                "call differs from bound actor, profile, or live control",
            ));
        }
        let identity = serde_json::to_vec(&(
            activation,
            request.provider_response_group,
            request.provider_call_index,
        ))
        .map_err(error)?;
        let invocation_id =
            InvocationId::new(format!("tool-{:x}", Sha256::digest(identity))).map_err(error)?;
        if snapshot
            .contract()
            .invocations()
            .iter()
            .any(|item| item.invocation_id == invocation_id)
        {
            return Err(error("invocation is already retained; no automatic replay"));
        }
        self.validate_human_tool_approval(activation, request)?;
        let preparation = self
            .authority
            .prepare_dispatch(
                &bound.lease,
                invocation_id.clone(),
                request.tool_call.name.clone(),
                DispatchReservation {
                    tokens: 0,
                    cost_microunits: 0,
                },
                now_ms()?,
            )
            .map_err(error)?;
        let bytes = serde_json::to_vec(&request.tool_call.arguments).map_err(error)?;
        // Validation above performs no durable mutation. A refused grant,
        // stale lease, or exhausted allowance must not poison readable history.
        // Once evidence reservation starts, any failed/uncertain write still
        // fences this controller until recovery.
        let result = (|| {
            let arguments = self
                .content
                .reserve_tool(
                    &snapshot,
                    activation,
                    invocation_id.clone(),
                    &bytes,
                    MAX_RESULT_BYTES,
                )
                .map_err(error)?;
            if self
                .content
                .read_tool_arguments(&arguments)
                .map_err(error)?
                != bytes
            {
                return Err(error("protected executable arguments changed"));
            }
            let replay_policy = if request.tool_call.name == delegate::NAME {
                self.delegate_replay_policy(activation, &invocation_id, request, &arguments)?
            } else {
                InvocationReplayPolicy::ManualOnly
            };
            let provider_run_ref = if request.tool_call.provider_metadata.is_empty() {
                None
            } else {
                Some(
                    self.content
                        .retain_activation_evidence(ActivationEvidenceContent::Attachment {
                            reference_id: invocation_id.as_str().to_owned(),
                            media_type: "application/vnd.axocoatl.provider-replay+json".into(),
                            text: serde_json::to_string(&request.tool_call.provider_metadata)
                                .map_err(error)?,
                        })
                        .map_err(error)?
                        .reference()
                        .clone(),
                )
            };
            self.append(
                &format!("intent:{}", invocation_id.as_str()),
                TurnContractEvent::RecordIntent {
                    invocation_id: invocation_id.clone(),
                    activation: activation.clone(),
                },
            )?;
            #[cfg(test)]
            self.trip(TestFailure::CanonicalIntent)?;
            let intent = InvocationIntent {
                invocation_id: invocation_id.clone(),
                activation: activation.clone(),
                dispatch_scope: preparation.dispatch_scope().to_string(),
                tool_name: request.tool_call.name.clone(),
                arguments: arguments.protected_arguments().clone(),
                redacted_preview: "[protected executable arguments]".into(),
                authority: InvocationAuthority {
                    grant_id: bound.grant.grant_id.as_str().to_owned(),
                    grant_revision: bound.grant.revision,
                    approval_ref: None,
                },
                replay_policy,
                provider_replay: ProviderReplayIdentity {
                    adapter_id: request.provider_id.clone(),
                    adapter_version: "actor-boundary-v1".into(),
                    provider_run_ref,
                    native_call_id: (!request.tool_call.id.is_empty())
                        .then(|| request.tool_call.id.clone()),
                    response_group_id: Some(format!(
                        "{}:{}:{}",
                        request.provider_response_group,
                        request.provider_call_index,
                        request.provider_call_count
                    )),
                },
            };
            let receipt = self
                .audit
                .record_intent(InvocationIntentCommand {
                    command_id: CommandId::new(format!("intent:{}", invocation_id.as_str()))
                        .map_err(error)?,
                    expected_revision: 0,
                    intent: intent.clone(),
                })
                .map_err(error)?;
            #[cfg(test)]
            self.trip(TestFailure::AuditIntent)?;
            self.authority
                .claim_dispatch(preparation, &receipt, &self.audit, now_ms()?)
                .map_err(error)?;
            #[cfg(test)]
            self.trip(TestFailure::AuthorityClaim)?;
            Ok((arguments, intent, bound.grant.evidence))
        })();
        self.fail_closed(result)
    }

    fn settle(
        &mut self,
        arguments: &DurableToolArguments,
        intent: &InvocationIntent,
        authority_ref: &EvidenceRef,
        outcome: &ToolInvocationOutcome,
    ) -> Result<()> {
        #[cfg(test)]
        if intent.tool_name == delegate::NAME {
            self.trip(TestFailure::DelegateOutcome)?;
        }
        let ToolInvocationOutcome::Returned(returned) = outcome else {
            return Err(error(
                "backend outcome is unknown; retained intent requires reconciliation",
            ));
        };
        let disposition = if returned.is_ok() {
            InvocationOutcome::Succeeded
        } else {
            InvocationOutcome::Failed
        };
        let bytes = serde_json::to_vec(returned).map_err(error)?;
        let result = self
            .content
            .record_tool_result(arguments, disposition, &bytes, now_ms()?)
            .map_err(error)?;
        #[cfg(test)]
        self.trip(TestFailure::ContentResult)?;
        self.audit
            .record_evidence(InvocationEvidenceCommand {
                command_id: CommandId::new(format!("outcome:{}", intent.invocation_id.as_str()))
                    .map_err(error)?,
                expected_revision: 1,
                invocation_id: intent.invocation_id.clone(),
                activation: intent.activation.clone(),
                authority: intent.authority.clone(),
                evidence: InvocationFinalEvidence::Outcome {
                    outcome: disposition,
                    result: result.protected_result().clone(),
                    redacted_preview: "[protected raw outcome]".into(),
                    source: InvocationOutcomeSource::Executor,
                    authority_ref: authority_ref.clone(),
                },
            })
            .map_err(error)?;
        #[cfg(test)]
        self.trip(TestFailure::AuditOutcome)?;
        let revision = self.authority.revision().map_err(error)?;
        self.authority
            .settle_dispatch(&intent.invocation_id, &self.audit, revision)
            .map_err(error)?;
        let snapshot = self.canonical.snapshot(&self.turn_id).map_err(error)?;
        if !snapshot
            .contract()
            .state()
            .is_some_and(LogicalTurnState::is_closed)
        {
            self.append(
                &format!("outcome:{}", intent.invocation_id.as_str()),
                TurnContractEvent::RecordOutcome {
                    invocation_id: intent.invocation_id.clone(),
                    outcome: disposition,
                    evidence: result.protected_result().evidence_ref.clone(),
                },
            )?;
        }
        Ok(())
    }
}

fn now_ms() -> Result<u64> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(error)?
        .as_millis();
    u64::try_from(elapsed).map_err(error)
}

#[cfg(all(test, unix))]
mod tests {
    include!("session_dispatch_tests.rs");
    mod lifecycle_tests {
        include!("session_dispatch_lifecycle_tests.rs");
    }
}

#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq)]
enum TestFailure {
    CanonicalIntent,
    AuditIntent,
    AuthorityClaim,
    ContentResult,
    DelegateOutcome,
    StreamObservation,
    AuditOutcome,
    CanonicalClose,
    Promotion,
    SuccessorRequest,
    SuccessorBegin,
    AgentCommandRequested,
    AgentCommandAccepted,
}
#[cfg(test)]
impl DispatchState {
    fn trip(&mut self, cut: TestFailure) -> Result<()> {
        if self.fail_at == Some(cut) {
            self.fail_at = None;
            Err(error("injected lost durable acknowledgement"))
        } else {
            Ok(())
        }
    }

    /// A process that dies while an Agent's command is at `cut` writes
    /// nothing more, so the delegate return is lost with it.
    fn crash_agent_command(
        &mut self,
        view: &axocoatl_session::control_command::CommandReceiptView,
        cut: TestFailure,
    ) -> Result<()> {
        if !matches!(
            view.source,
            axocoatl_session::control_command::CommandSourceRecord::Agent { .. }
        ) {
            return Ok(());
        }
        let result = self.trip(cut);
        if result.is_err() {
            self.fail_at = Some(TestFailure::DelegateOutcome);
        }
        self.fail_closed(result)
    }
}

#[cfg(test)]
impl SessionDispatchController {
    pub(crate) fn lose_delegate_outcome_for_test(&self) {
        self.lock().unwrap().fail_at = Some(TestFailure::DelegateOutcome);
    }

    /// Stop the process while the lead's first helper admission is recorded
    /// as requested (`accepted: false`) or accepted but not yet applied.
    pub(crate) fn crash_delegate_admission_for_test(&self, accepted: bool) {
        self.lock().unwrap().fail_at = Some(if accepted {
            TestFailure::AgentCommandAccepted
        } else {
            TestFailure::AgentCommandRequested
        });
    }
}
