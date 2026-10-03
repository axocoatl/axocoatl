//! Host-invocation tools bound to one exact invocation.
//!
//! `web_search`, `web_fetch`, `browser` and `browser_check` run in the
//! daemon or in a container it manages, not in the Session's repository
//! container, and each call must know which tool call it is: its records and
//! egress bindings are attributed to that invocation and its activation. The
//! repository tools get the same exactness from `RepositoryInvocation`; this
//! is the equivalent seam for host tools.
//!
//! A host tool is offered only when the Agent's `tools` list names it (the
//! exact-list rule) and only when the tool says it is available for that
//! Agent: [`HostInvocationTool::refusal`] is checked when the team is
//! admitted, when an activation is prepared and again when a call is
//! admitted. The tool the model sees during preparation is a description
//! only; admission replaces it with [`HostInvocationTool::bind`] for the
//! admitted invocation, behind a gate that runs it once, with exactly the
//! admitted and audited arguments, while its activation is still current.
//!
//! A tool can be left out instead of refused. A controller may withhold it
//! ([`HostInvocationTool::withheld`]), and a tool may refuse Ways attempt
//! lanes ([`HostInvocationTool::attempt_refusal`]). Either way an activation
//! that lists it is prepared without it, as if it were not listed, and a call
//! to it anyway is declined with its reason.
use super::*;
use axocoatl_tools::{BuiltinTool, ToolError, ToolExecutor};
use std::sync::atomic::{AtomicBool, Ordering};

/// Every host tool name. Listing one is syntactically valid in a native
/// Session; whether it is available is the registered tool's decision.
pub(crate) const HOST_INVOCATION_TOOLS: [&str; 5] = [
    "web_search",
    "web_fetch",
    "browser",
    "browser_check",
    "request_network_access",
];

pub(crate) fn is_host_invocation_tool(name: &str) -> bool {
    HOST_INVOCATION_TOOLS.contains(&name)
}

/// The exact call a bound host tool serves.
#[derive(Clone)]
pub(crate) struct HostInvocationContext {
    pub session_id: String,
    pub invocation_id: InvocationId,
    pub activation: ActivationRef,
    /// The Agent's definition id.
    pub agent: String,
    /// The activation was admitted with an empty write scope.
    pub read_only: bool,
    /// The host directory of the activation's checkout, when it has one.
    pub checkout: Option<axocoatl_core::SecureDir>,
    /// The activation runs in a Ways attempt lane, on that attempt's own
    /// checkout and container.
    pub attempt: bool,
}

impl std::fmt::Debug for HostInvocationContext {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HostInvocationContext")
            .field("session_id", &self.session_id)
            .field("invocation_id", &self.invocation_id.as_str())
            .field("activation", &self.activation.activation_id.as_str())
            .field("agent", &self.agent)
            .field("read_only", &self.read_only)
            .field("attempt", &self.attempt)
            .finish_non_exhaustive()
    }
}

/// A tool the host runs for one admitted invocation.
pub(crate) trait HostInvocationTool: Send + Sync {
    fn name(&self) -> &'static str;
    /// Description and schema for the model. Executing it fails: only a
    /// bound tool runs.
    fn definition(&self) -> Arc<dyn BuiltinTool>;
    /// Why this tool is not available to an activation with `profile`, or
    /// `None` when it is.
    fn refusal(&self, profile: &ExecutionProfile) -> Option<String>;
    /// Whether this controller's activations go without this tool: it is
    /// not offered, and listing it does not stop an activation. A call to it
    /// anyway is declined with [`Self::refusal`].
    fn withheld(&self) -> bool {
        false
    }
    /// Why the tool cannot run in a Ways attempt lane, if it cannot. An
    /// attempt activation that lists it runs without it.
    fn attempt_refusal(&self) -> Option<String> {
        None
    }
    /// The tool for exactly this invocation.
    fn bind(&self, context: HostInvocationContext) -> Arc<dyn BuiltinTool>;
}

/// The host tools registered on one controller, by name.
pub(crate) type HostTools = HashMap<&'static str, Arc<dyn HostInvocationTool>>;

/// A host tool's description, offered to the model before admission. It
/// never runs anything.
pub(crate) struct HostToolDefinition {
    name: &'static str,
    description: String,
    schema: serde_json::Value,
    policy: axocoatl_llm::ConcurrencyPolicy,
}

impl HostToolDefinition {
    pub(crate) fn new(
        name: &'static str,
        description: impl Into<String>,
        schema: serde_json::Value,
        policy: axocoatl_llm::ConcurrencyPolicy,
    ) -> Self {
        Self {
            name,
            description: description.into(),
            schema,
            policy,
        }
    }
}

#[async_trait]
impl BuiltinTool for HostToolDefinition {
    fn description(&self) -> &str {
        &self.description
    }

    fn parameters_schema(&self) -> serde_json::Value {
        self.schema.clone()
    }

    fn concurrency_policy(&self) -> axocoatl_llm::ConcurrencyPolicy {
        self.policy
    }

    async fn execute(
        &self,
        _arguments: serde_json::Value,
    ) -> std::result::Result<serde_json::Value, ToolError> {
        Err(ToolError::ExecutionFailed {
            tool: self.name.to_string(),
            reason: "a host tool runs only through its admitted invocation".into(),
        })
    }
}

/// Whether an activation that lists `tool` runs without it: the controller
/// withholds it, or it refuses the Ways attempt lane the activation runs in.
fn left_out(tool: &dyn HostInvocationTool, attempt: bool) -> bool {
    tool.withheld() || (attempt && tool.attempt_refusal().is_some())
}

/// The first reason any host tool listed in `profile` is unavailable outside
/// a Ways attempt. A withheld tool is no reason: the activation runs without
/// it.
pub(crate) fn host_tool_refusal(
    registered: &HostTools,
    profile: &ExecutionProfile,
) -> Option<String> {
    listed_host_tool_refusal(registered, profile, false)
}

/// [`host_tool_refusal`] for an activation that runs in a Ways attempt lane
/// when `attempt` is set.
fn listed_host_tool_refusal(
    registered: &HostTools,
    profile: &ExecutionProfile,
    attempt: bool,
) -> Option<String> {
    profile
        .tools
        .iter()
        .filter(|tool| is_host_invocation_tool(tool))
        .find_map(|tool| match registered.get(tool.as_str()) {
            None => Some(format!(
                "{tool} is listed for {} but this daemon does not provide {tool}",
                profile.definition
            )),
            Some(host) if left_out(host.as_ref(), attempt) => None,
            Some(host) => host.refusal(profile),
        })
}

/// Whether an activation bound to `repository` runs in a Ways attempt lane.
pub(super) fn bound_to_attempt(
    repository: Option<&super::repository_activation::RepositoryActivationResource>,
) -> bool {
    repository.is_some_and(|resource| resource.is_attempt())
}

/// Why `tool` cannot run for this activation: the tool's own refusal, or its
/// refusal of Ways attempt lanes.
fn host_tool_refused(
    tool: &dyn HostInvocationTool,
    profile: &ExecutionProfile,
    attempt: bool,
) -> Option<String> {
    tool.refusal(profile).or_else(|| {
        if attempt {
            tool.attempt_refusal()
        } else {
            None
        }
    })
}

impl SessionDispatchController {
    /// Make a host tool available to this controller's activations. Calling
    /// it again for the same name replaces the earlier registration.
    pub(crate) fn register_host_invocation_tool(
        &self,
        tool: Arc<dyn HostInvocationTool>,
    ) -> Result<()> {
        if !is_host_invocation_tool(tool.name()) {
            return Err(error(format!(
                "{} is not a host invocation tool",
                tool.name()
            )));
        }
        self.lock()?.host_tools.insert(tool.name(), tool);
        Ok(())
    }
}

impl DispatchState {
    /// Why an activation with `profile` cannot have the host tools it lists.
    /// Tools left out of a Ways attempt (`attempt`) are no reason.
    pub(super) fn host_tool_refusal(
        &self,
        profile: &ExecutionProfile,
        attempt: bool,
    ) -> Option<String> {
        listed_host_tool_refusal(&self.host_tools, profile, attempt)
    }

    /// Descriptions of the host tools `profile` lists, for the model. A
    /// withheld tool, or one that refuses this Ways attempt, is not offered.
    pub(super) fn host_tool_definitions(
        &self,
        profile: &ExecutionProfile,
        attempt: bool,
    ) -> Vec<(&'static str, Arc<dyn BuiltinTool>)> {
        profile
            .tools
            .iter()
            .filter_map(|tool| self.host_tools.get(tool.as_str()))
            .filter(|tool| !left_out(tool.as_ref(), attempt))
            .filter(|tool| tool.refusal(profile).is_none())
            .map(|tool| (tool.name(), tool.definition()))
            .collect()
    }

    /// Before any durable admission: decline a call to a listed host tool
    /// that is not available, so the model gets the reason and nothing is
    /// recorded. Unlisted names are left to authority, which refuses them.
    pub(super) fn host_tool_admission_refusal(
        &self,
        activation: &ActivationRef,
        tool: &str,
    ) -> Option<String> {
        if !is_host_invocation_tool(tool) {
            return None;
        }
        let bound = self
            .bound
            .get(&activation.activation_id)
            .filter(|bound| bound.activation == *activation)?;
        if !bound.profile.tools.iter().any(|listed| listed == tool) {
            return None;
        }
        match self.host_tools.get(tool) {
            None => Some(format!(
                "{tool} is listed for {} but this daemon does not provide {tool}",
                bound.profile.definition
            )),
            Some(host) => host_tool_refused(
                host.as_ref(),
                &bound.profile,
                bound_to_attempt(bound.repository.as_ref()),
            ),
        }
    }

    /// The executor for one admitted host-tool invocation, or `None` when
    /// the call is not to a host tool.
    pub(super) fn host_invocation_executor(
        &self,
        controller: &SessionDispatchController,
        intent: &InvocationIntent,
    ) -> Result<Option<Arc<ToolExecutor>>> {
        if !is_host_invocation_tool(&intent.tool_name) {
            return Ok(None);
        }
        let bound = self
            .bound
            .get(&intent.activation.activation_id)
            .filter(|bound| bound.activation == intent.activation)
            .ok_or_else(|| error("host tool invocation has no exact bound activation"))?;
        let tool = self
            .host_tools
            .get(intent.tool_name.as_str())
            .ok_or_else(|| {
                error(format!(
                    "{} is not provided by this daemon",
                    intent.tool_name
                ))
            })?;
        let attempt = bound_to_attempt(bound.repository.as_ref());
        if let Some(reason) = host_tool_refused(tool.as_ref(), &bound.profile, attempt) {
            return Err(error(reason));
        }
        let context = HostInvocationContext {
            session_id: intent.activation.session_id.as_str().to_owned(),
            invocation_id: intent.invocation_id.clone(),
            activation: intent.activation.clone(),
            agent: bound.profile.definition.clone(),
            read_only: self
                .admitted_write_scope(&intent.activation)?
                .is_read_only(),
            checkout: bound
                .repository
                .as_ref()
                .map(|resource| resource.host_checkout()),
            attempt,
        };
        let mut executor = ToolExecutor::new();
        executor.register_builtin(
            tool.name(),
            Arc::new(HostInvocationGate {
                controller: controller.clone(),
                intent: intent.clone(),
                called: AtomicBool::new(false),
                tool: tool.bind(context),
            }),
        );
        Ok(Some(Arc::new(executor)))
    }
}

/// Runs the bound tool once, with exactly the admitted arguments, while its
/// activation is still current.
struct HostInvocationGate {
    controller: SessionDispatchController,
    intent: InvocationIntent,
    called: AtomicBool,
    tool: Arc<dyn BuiltinTool>,
}

impl HostInvocationGate {
    fn validate(&self, state: &DispatchState) -> Result<()> {
        state.execution_admission()?;
        state.current(&self.intent.activation)?;
        let bound = state
            .bound
            .get(&self.intent.activation.activation_id)
            .filter(|bound| {
                bound.activation == self.intent.activation && !bound.control.is_cancelled()
            })
            .ok_or_else(|| error("host tool invocation is no longer current"))?;
        state
            .authority
            .validate_claimed_dispatch(&bound.lease, &self.intent, &state.audit, now_ms()?)
            .map_err(error)
    }

    fn begin(&self, arguments: &serde_json::Value) -> Result<()> {
        let bytes = serde_json::to_vec(arguments).map_err(error)?;
        let state = self.controller.lock()?;
        self.validate(&state)?;
        let snapshot = state.current(&self.intent.activation)?;
        let stored = state
            .content
            .tool_arguments(
                &snapshot,
                &self.intent.activation,
                &self.intent.invocation_id,
            )
            .map_err(error)?
            .ok_or_else(|| error("protected host tool arguments are missing"))?;
        if stored.protected_arguments() != &self.intent.arguments
            || state.content.read_tool_arguments(&stored).map_err(error)? != bytes
        {
            return Err(error(
                "host tool arguments differ from acknowledged admission",
            ));
        }
        if self.called.swap(true, Ordering::AcqRel) {
            return Err(error(
                "host tool invocation executor was already consumed; no replay",
            ));
        }
        Ok(())
    }
}

#[async_trait]
impl BuiltinTool for HostInvocationGate {
    fn description(&self) -> &str {
        self.tool.description()
    }

    fn parameters_schema(&self) -> serde_json::Value {
        self.tool.parameters_schema()
    }

    fn concurrency_policy(&self) -> axocoatl_llm::ConcurrencyPolicy {
        self.tool.concurrency_policy()
    }

    async fn execute(
        &self,
        arguments: serde_json::Value,
    ) -> std::result::Result<serde_json::Value, ToolError> {
        self.begin(&arguments)
            .map_err(|failure| ToolError::ExecutionFailed {
                tool: self.intent.tool_name.clone(),
                reason: failure.to_string(),
            })?;
        self.tool.execute(arguments).await
    }
}
