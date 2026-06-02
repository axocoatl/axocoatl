//! Tool execution hooks — pre/post execution extensibility points.

use serde::{Deserialize, Serialize};

/// Phase of hook execution relative to the tool call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HookPhase {
    /// Before tool execution. Can deny or transform arguments.
    Pre,
    /// After tool execution. Can transform results.
    Post,
}

/// Action a hook returns to control the tool execution pipeline.
#[derive(Debug, Clone)]
pub enum HookAction {
    /// Allow the tool call to proceed (or pass through the result).
    Allow,
    /// Deny the tool call with a reason (Pre only).
    Deny { reason: String },
    /// Transform the arguments (Pre) or result (Post).
    Transform { value: serde_json::Value },
}

/// Context passed to hooks for decision-making.
#[derive(Debug, Clone)]
pub struct HookContext {
    pub tool_name: String,
    pub phase: HookPhase,
    pub agent_id: String,
    /// For Pre hooks: the tool arguments. For Post hooks: the tool result.
    pub value: serde_json::Value,
}

/// Trait for tool execution hooks.
#[async_trait::async_trait]
pub trait ToolHook: Send + Sync + 'static {
    /// Unique name for this hook.
    fn name(&self) -> &str;

    /// Which phase(s) this hook applies to.
    fn phases(&self) -> Vec<HookPhase>;

    /// Optional: only apply to specific tool names. Empty = all tools.
    fn tool_filter(&self) -> Vec<String> {
        Vec::new()
    }

    /// Execute the hook and return an action.
    async fn execute(&self, ctx: &HookContext) -> HookAction;
}

/// A hook that logs all tool executions.
pub struct LoggingHook;

#[async_trait::async_trait]
impl ToolHook for LoggingHook {
    fn name(&self) -> &str {
        "logging"
    }

    fn phases(&self) -> Vec<HookPhase> {
        vec![HookPhase::Pre, HookPhase::Post]
    }

    async fn execute(&self, ctx: &HookContext) -> HookAction {
        match ctx.phase {
            HookPhase::Pre => {
                tracing::info!(
                    tool = %ctx.tool_name,
                    agent = %ctx.agent_id,
                    "Tool execution starting"
                );
            }
            HookPhase::Post => {
                tracing::info!(
                    tool = %ctx.tool_name,
                    agent = %ctx.agent_id,
                    "Tool execution completed"
                );
            }
        }
        HookAction::Allow
    }
}

/// A hook that denies specific tools.
pub struct DenyListHook {
    denied_tools: Vec<String>,
}

impl DenyListHook {
    pub fn new(denied_tools: Vec<String>) -> Self {
        Self { denied_tools }
    }
}

#[async_trait::async_trait]
impl ToolHook for DenyListHook {
    fn name(&self) -> &str {
        "deny_list"
    }

    fn phases(&self) -> Vec<HookPhase> {
        vec![HookPhase::Pre]
    }

    async fn execute(&self, ctx: &HookContext) -> HookAction {
        if self.denied_tools.contains(&ctx.tool_name) {
            HookAction::Deny {
                reason: format!("Tool '{}' is denied by policy", ctx.tool_name),
            }
        } else {
            HookAction::Allow
        }
    }
}

/// A hook that enforces argument size limits.
pub struct ArgSizeLimitHook {
    max_bytes: usize,
}

impl ArgSizeLimitHook {
    pub fn new(max_bytes: usize) -> Self {
        Self { max_bytes }
    }
}

#[async_trait::async_trait]
impl ToolHook for ArgSizeLimitHook {
    fn name(&self) -> &str {
        "arg_size_limit"
    }

    fn phases(&self) -> Vec<HookPhase> {
        vec![HookPhase::Pre]
    }

    async fn execute(&self, ctx: &HookContext) -> HookAction {
        let size = serde_json::to_string(&ctx.value)
            .map(|s| s.len())
            .unwrap_or(0);
        if size > self.max_bytes {
            HookAction::Deny {
                reason: format!(
                    "Arguments for '{}' exceed size limit: {} > {} bytes",
                    ctx.tool_name, size, self.max_bytes
                ),
            }
        } else {
            HookAction::Allow
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn logging_hook_allows() {
        let hook = LoggingHook;
        let ctx = HookContext {
            tool_name: "echo".to_string(),
            phase: HookPhase::Pre,
            agent_id: "test".to_string(),
            value: serde_json::json!({}),
        };
        assert!(matches!(hook.execute(&ctx).await, HookAction::Allow));
    }

    #[tokio::test]
    async fn deny_list_blocks_tool() {
        let hook = DenyListHook::new(vec!["dangerous".to_string()]);
        let ctx = HookContext {
            tool_name: "dangerous".to_string(),
            phase: HookPhase::Pre,
            agent_id: "test".to_string(),
            value: serde_json::json!({}),
        };
        assert!(matches!(hook.execute(&ctx).await, HookAction::Deny { .. }));
    }

    #[tokio::test]
    async fn deny_list_allows_safe_tool() {
        let hook = DenyListHook::new(vec!["dangerous".to_string()]);
        let ctx = HookContext {
            tool_name: "echo".to_string(),
            phase: HookPhase::Pre,
            agent_id: "test".to_string(),
            value: serde_json::json!({}),
        };
        assert!(matches!(hook.execute(&ctx).await, HookAction::Allow));
    }

    #[tokio::test]
    async fn arg_size_limit_blocks_large() {
        let hook = ArgSizeLimitHook::new(10);
        let ctx = HookContext {
            tool_name: "echo".to_string(),
            phase: HookPhase::Pre,
            agent_id: "test".to_string(),
            value: serde_json::json!({"text": "this is a very long string that exceeds the limit"}),
        };
        assert!(matches!(hook.execute(&ctx).await, HookAction::Deny { .. }));
    }

    #[tokio::test]
    async fn arg_size_limit_allows_small() {
        let hook = ArgSizeLimitHook::new(1000);
        let ctx = HookContext {
            tool_name: "echo".to_string(),
            phase: HookPhase::Pre,
            agent_id: "test".to_string(),
            value: serde_json::json!({"text": "hi"}),
        };
        assert!(matches!(hook.execute(&ctx).await, HookAction::Allow));
    }
}
