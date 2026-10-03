//! Host-invocation tools: tools the daemon runs for one exact tool call, on
//! the host or in a container it manages, rather than in the Session's
//! repository. Each call gets its own executor bound to its invocation id,
//! activation and Agent, the way repository tools do, so what it does can be
//! attributed to that call in the Session's records.
//!
//! A tool is offered only when the Agent's `tools` list names it, and
//! admission refuses a listed tool the daemon cannot run, with its reason.
//! The bound executor runs once, and only with the exact arguments that were
//! admitted and audited.
use super::*;
use axocoatl_tools::{BuiltinTool, ToolError, ToolExecutor};
use std::sync::atomic::{AtomicBool, Ordering};

/// Every name the host-invocation path may serve. A name is offered only
/// when a tool for it is registered on the controller.
pub(crate) const HOST_INVOCATION_TOOLS: [&str; 4] =
    ["web_search", "web_fetch", "browser", "browser_check"];

/// The exact call a bound host tool serves.
#[derive(Clone)]
pub(crate) struct HostInvocationContext {
    pub session_id: String,
    pub invocation_id: axocoatl_session::turn_contract::InvocationId,
    pub activation: ActivationRef,
    /// The Agent definition id.
    pub agent: String,
    pub read_only: bool,
    /// The host directory of the activation's checkout, when it has one.
    pub checkout: Option<axocoatl_core::SecureDir>,
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
            .finish_non_exhaustive()
    }
}

/// A tool the daemon serves per invocation.
pub(crate) trait HostInvocationTool: Send + Sync {
    fn name(&self) -> &'static str;
    /// Description and schema; executing it fails.
    fn definition(&self) -> Arc<dyn BuiltinTool>;
    /// Why this daemon cannot run the tool for this profile, if it cannot.
    fn refusal(&self, profile: &ExecutionProfile) -> Option<String>;
    /// The tool bound to one exact call.
    fn bind(&self, context: HostInvocationContext) -> Arc<dyn BuiltinTool>;
}

pub(super) type HostTools = Vec<Arc<dyn HostInvocationTool>>;

impl SessionDispatchController {
    /// Register (or replace, by name) a host-invocation tool.
    pub(crate) fn register_host_invocation_tool(
        &self,
        tool: Arc<dyn HostInvocationTool>,
    ) -> Result<()> {
        if !HOST_INVOCATION_TOOLS.contains(&tool.name()) {
            return Err(error(format!(
                "{} is not a host-invocation tool",
                tool.name()
            )));
        }
        let mut state = self.lock()?;
        state
            .host_tools
            .retain(|existing| existing.name() != tool.name());
        state.host_tools.push(tool);
        Ok(())
    }
}

impl DispatchState {
    pub(super) fn host_tool(&self, name: &str) -> Option<Arc<dyn HostInvocationTool>> {
        self.host_tools
            .iter()
            .find(|tool| tool.name() == name)
            .cloned()
    }

    /// The definitions of the registered host tools this profile lists, for
    /// the request's tool list. Refused tools are not offered.
    pub(super) fn host_tool_definitions(
        &self,
        profile: &ExecutionProfile,
    ) -> Vec<(&'static str, Arc<dyn BuiltinTool>)> {
        self.host_tools
            .iter()
            .filter(|tool| profile.tools.iter().any(|listed| listed == tool.name()))
            .filter(|tool| tool.refusal(profile).is_none())
            .map(|tool| (tool.name(), tool.definition()))
            .collect()
    }

    /// Why a listed host tool cannot be admitted for this activation.
    pub(super) fn host_tool_refusal(
        &self,
        activation: &ActivationRef,
        tool_name: &str,
    ) -> Result<Option<String>> {
        if !HOST_INVOCATION_TOOLS.contains(&tool_name) {
            return Ok(None);
        }
        let bound = self
            .bound
            .get(&activation.activation_id)
            .filter(|bound| bound.activation == *activation)
            .ok_or_else(|| error("invocation has no exact bound executor"))?;
        if !bound.profile.tools.iter().any(|listed| listed == tool_name) {
            return Ok(Some(format!("{tool_name} is not in this Agent's tools")));
        }
        Ok(match self.host_tool(tool_name) {
            None => Some(format!(
                "{tool_name} is listed for {} but this daemon does not provide it",
                bound.profile.definition
            )),
            Some(tool) => tool.refusal(&bound.profile),
        })
    }

    /// The executor for one admitted host-tool call, or `None` for any other
    /// tool.
    pub(super) fn host_invocation_executor(
        &self,
        controller: &SessionDispatchController,
        intent: &InvocationIntent,
    ) -> Result<Option<Arc<ToolExecutor>>> {
        let Some(tool) = self.host_tool(&intent.tool_name) else {
            return Ok(None);
        };
        let bound = self
            .bound
            .get(&intent.activation.activation_id)
            .filter(|bound| bound.activation == intent.activation)
            .ok_or_else(|| error("invocation has no exact bound executor"))?;
        if let Some(reason) = tool.refusal(&bound.profile) {
            return Err(error(reason));
        }
        let context = HostInvocationContext {
            session_id: intent.activation.session_id.as_str().to_string(),
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
        };
        let mut executor = ToolExecutor::new();
        executor.register_builtin(
            intent.tool_name.clone(),
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
