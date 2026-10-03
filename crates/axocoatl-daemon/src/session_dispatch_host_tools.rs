//! Host tools bound to one exact invocation.
//!
//! `web_search`, `web_fetch` and `browser` run in the daemon, not in the
//! Session container, and each call must know which tool call it is: its
//! records and egress bindings are attributed to that invocation and its
//! activation. The repository tools get the same exactness from
//! `RepositoryInvocation`; this is the equivalent seam for host tools.
//!
//! A host tool is offered only when the Agent's `tools` list names it (the
//! exact-list rule) and only when the tool says it is available for that
//! Agent: [`HostInvocationTool::refusal`] is checked when the team is
//! admitted, when an activation is prepared and again when a call is
//! admitted. The tool the model sees during preparation is a description
//! only; admission replaces it with [`HostInvocationTool::bind`] for the
//! admitted invocation.
use super::*;
use axocoatl_tools::{BuiltinTool, ToolError, ToolExecutor};

/// Every host tool name. Listing one is syntactically valid in a native
/// Session; whether it is available is the registered tool's decision.
pub(crate) const HOST_INVOCATION_TOOLS: [&str; 3] = ["web_search", "web_fetch", "browser"];

pub(crate) fn is_host_invocation_tool(name: &str) -> bool {
    HOST_INVOCATION_TOOLS.contains(&name)
}

/// The exact call a bound host tool serves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HostInvocationContext {
    pub session_id: String,
    pub invocation_id: InvocationId,
    pub activation: ActivationRef,
    /// The Agent's definition id.
    pub agent: String,
    /// The activation was admitted with an empty write scope.
    pub read_only: bool,
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
    /// The tool for exactly this invocation.
    fn bind(&self, context: HostInvocationContext) -> Arc<dyn BuiltinTool>;
}

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

/// The first reason any host tool listed in `profile` is unavailable.
pub(crate) fn host_tool_refusal(
    registered: &HashMap<&'static str, Arc<dyn HostInvocationTool>>,
    profile: &ExecutionProfile,
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
            Some(host) => host.refusal(profile),
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
    pub(super) fn host_tool_refusal(&self, profile: &ExecutionProfile) -> Option<String> {
        host_tool_refusal(&self.host_tools, profile)
    }

    /// Descriptions of the host tools `profile` lists, for the model.
    pub(super) fn host_tool_definitions(
        &self,
        profile: &ExecutionProfile,
    ) -> Vec<(&'static str, Arc<dyn BuiltinTool>)> {
        profile
            .tools
            .iter()
            .filter_map(|tool| self.host_tools.get(tool.as_str()))
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
            Some(host) => host.refusal(&bound.profile),
        }
    }

    /// The executor for one admitted host-tool invocation, or `None` when
    /// the call is not to a host tool.
    pub(super) fn host_invocation_executor(
        &self,
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
        let context = HostInvocationContext {
            session_id: intent.activation.session_id.as_str().to_owned(),
            invocation_id: intent.invocation_id.clone(),
            activation: intent.activation.clone(),
            agent: bound.profile.definition.clone(),
            read_only: bound
                .profile
                .write_scope
                .as_ref()
                .is_some_and(Vec::is_empty),
        };
        let mut executor = ToolExecutor::new();
        executor.register_builtin(tool.name(), tool.bind(context));
        Ok(Some(Arc::new(executor)))
    }
}
