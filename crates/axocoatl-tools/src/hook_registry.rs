//! Registry for tool execution hooks.
//! Manages global and per-tool hooks with timeout enforcement.

use std::sync::Arc;
use std::time::Duration;

use crate::hooks::{HookAction, HookContext, HookPhase, ToolHook};

/// Configuration for hook execution.
#[derive(Debug, Clone)]
pub struct HookConfig {
    /// Maximum time a single hook may take before being killed.
    pub timeout: Duration,
    /// Maximum hook chain depth (prevents hooks triggering hooks).
    pub max_depth: usize,
}

impl Default for HookConfig {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(30),
            max_depth: 1,
        }
    }
}

/// Registry of tool execution hooks.
pub struct HookRegistry {
    /// Hooks that apply to all tools.
    global_hooks: Vec<Arc<dyn ToolHook>>,
    /// Hooks that apply to specific tools (tool_name → hooks).
    tool_hooks: std::collections::HashMap<String, Vec<Arc<dyn ToolHook>>>,
    config: HookConfig,
}

impl HookRegistry {
    pub fn new() -> Self {
        Self {
            global_hooks: Vec::new(),
            tool_hooks: std::collections::HashMap::new(),
            config: HookConfig::default(),
        }
    }

    pub fn with_config(config: HookConfig) -> Self {
        Self {
            global_hooks: Vec::new(),
            tool_hooks: std::collections::HashMap::new(),
            config,
        }
    }

    /// Register a global hook (applies to all tools).
    pub fn register_global(&mut self, hook: Arc<dyn ToolHook>) {
        self.global_hooks.push(hook);
    }

    /// Register a hook for a specific tool.
    pub fn register_for_tool(&mut self, tool_name: impl Into<String>, hook: Arc<dyn ToolHook>) {
        self.tool_hooks
            .entry(tool_name.into())
            .or_default()
            .push(hook);
    }

    /// Run all applicable pre-hooks for a tool call.
    /// Returns the final action (Allow, Deny, or Transform).
    /// If any hook returns Deny, execution stops immediately.
    /// If any hook returns Transform, subsequent hooks see the transformed value.
    pub async fn run_pre_hooks(
        &self,
        tool_name: &str,
        agent_id: &str,
        mut arguments: serde_json::Value,
    ) -> (HookAction, serde_json::Value) {
        let hooks = self.hooks_for(tool_name, HookPhase::Pre);

        for hook in hooks {
            let ctx = HookContext {
                tool_name: tool_name.to_string(),
                phase: HookPhase::Pre,
                agent_id: agent_id.to_string(),
                value: arguments.clone(),
            };

            let action = match tokio::time::timeout(self.config.timeout, hook.execute(&ctx)).await {
                Ok(action) => action,
                Err(_) => {
                    tracing::warn!(
                        hook = %hook.name(),
                        tool = %tool_name,
                        "Hook timed out, allowing"
                    );
                    HookAction::Allow
                }
            };

            match action {
                HookAction::Allow => continue,
                HookAction::Deny { reason } => {
                    return (HookAction::Deny { reason }, arguments);
                }
                HookAction::Transform { value } => {
                    arguments = value;
                }
            }
        }

        (HookAction::Allow, arguments)
    }

    /// Run all applicable post-hooks for a tool result.
    pub async fn run_post_hooks(
        &self,
        tool_name: &str,
        agent_id: &str,
        mut result: serde_json::Value,
    ) -> serde_json::Value {
        let hooks = self.hooks_for(tool_name, HookPhase::Post);

        for hook in hooks {
            let ctx = HookContext {
                tool_name: tool_name.to_string(),
                phase: HookPhase::Post,
                agent_id: agent_id.to_string(),
                value: result.clone(),
            };

            let action = match tokio::time::timeout(self.config.timeout, hook.execute(&ctx)).await {
                Ok(action) => action,
                Err(_) => {
                    tracing::warn!(hook = %hook.name(), "Post-hook timed out");
                    HookAction::Allow
                }
            };

            match action {
                HookAction::Allow => continue,
                HookAction::Transform { value } => {
                    result = value;
                }
                HookAction::Deny { .. } => {
                    // Post hooks can't deny — ignore
                    tracing::warn!(hook = %hook.name(), "Post-hook returned Deny, ignoring");
                }
            }
        }

        result
    }

    /// Collect all hooks applicable to a tool+phase.
    fn hooks_for(&self, tool_name: &str, phase: HookPhase) -> Vec<Arc<dyn ToolHook>> {
        let mut hooks: Vec<Arc<dyn ToolHook>> = Vec::new();

        // Global hooks first
        for hook in &self.global_hooks {
            if hook.phases().contains(&phase) {
                let filter = hook.tool_filter();
                if filter.is_empty() || filter.iter().any(|f| f == tool_name) {
                    hooks.push(hook.clone());
                }
            }
        }

        // Tool-specific hooks
        if let Some(tool_hooks) = self.tool_hooks.get(tool_name) {
            for hook in tool_hooks {
                if hook.phases().contains(&phase) {
                    hooks.push(hook.clone());
                }
            }
        }

        hooks
    }

    /// Number of registered hooks.
    pub fn hook_count(&self) -> usize {
        self.global_hooks.len() + self.tool_hooks.values().map(|v| v.len()).sum::<usize>()
    }
}

impl Default for HookRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::{DenyListHook, LoggingHook};

    #[tokio::test]
    async fn empty_registry_allows() {
        let reg = HookRegistry::new();
        let (action, _) = reg
            .run_pre_hooks("echo", "agent-1", serde_json::json!({}))
            .await;
        assert!(matches!(action, HookAction::Allow));
    }

    #[tokio::test]
    async fn global_logging_hook() {
        let mut reg = HookRegistry::new();
        reg.register_global(Arc::new(LoggingHook));

        let (action, _) = reg
            .run_pre_hooks("echo", "agent-1", serde_json::json!({"text": "hi"}))
            .await;
        assert!(matches!(action, HookAction::Allow));
    }

    #[tokio::test]
    async fn deny_list_hook_blocks() {
        let mut reg = HookRegistry::new();
        reg.register_global(Arc::new(DenyListHook::new(vec!["shell".to_string()])));

        let (action, _) = reg
            .run_pre_hooks("shell", "agent-1", serde_json::json!({"cmd": "rm -rf /"}))
            .await;
        assert!(matches!(action, HookAction::Deny { .. }));

        let (action, _) = reg
            .run_pre_hooks("echo", "agent-1", serde_json::json!({}))
            .await;
        assert!(matches!(action, HookAction::Allow));
    }

    #[tokio::test]
    async fn tool_specific_hook() {
        let mut reg = HookRegistry::new();
        reg.register_for_tool(
            "echo",
            Arc::new(DenyListHook::new(vec!["echo".to_string()])),
        );

        // echo is denied
        let (action, _) = reg
            .run_pre_hooks("echo", "agent-1", serde_json::json!({}))
            .await;
        assert!(matches!(action, HookAction::Deny { .. }));

        // other tools are fine
        let (action, _) = reg
            .run_pre_hooks("search", "agent-1", serde_json::json!({}))
            .await;
        assert!(matches!(action, HookAction::Allow));
    }

    #[tokio::test]
    async fn hook_count() {
        let mut reg = HookRegistry::new();
        assert_eq!(reg.hook_count(), 0);

        reg.register_global(Arc::new(LoggingHook));
        assert_eq!(reg.hook_count(), 1);

        reg.register_for_tool("echo", Arc::new(LoggingHook));
        assert_eq!(reg.hook_count(), 2);
    }
}
