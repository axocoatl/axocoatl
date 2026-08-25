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

            // Hooks are user-configurable extension code. Run each one in an
            // owned task so a panic cannot unwind the AgentActor or orphan the
            // enclosing tool-call evidence group.
            let hook_name = hook.name().to_string();
            let mut task = tokio::spawn(async move { hook.execute(&ctx).await });
            let action = match tokio::time::timeout(self.config.timeout, &mut task).await {
                Ok(Ok(action)) => action,
                Ok(Err(error)) => HookAction::Deny {
                    reason: format!(
                        "Pre-hook {hook_name} panicked before {tool_name} could execute: {error}"
                    ),
                },
                Err(_) => {
                    task.abort();
                    let _ = task.await;
                    tracing::warn!(
                        hook = %hook_name,
                        tool = %tool_name,
                        "Pre-hook timed out, denying tool execution"
                    );
                    HookAction::Deny {
                        reason: format!(
                            "Pre-hook {hook_name} timed out before {tool_name} could execute"
                        ),
                    }
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

            let hook_name = hook.name().to_string();
            let mut task = tokio::spawn(async move { hook.execute(&ctx).await });
            let action = match tokio::time::timeout(self.config.timeout, &mut task).await {
                Ok(Ok(action)) => action,
                Ok(Err(error)) => {
                    return serde_json::json!({
                        "error": format!(
                            "Post-hook {hook_name} panicked after {tool_name} executed: {error}"
                        )
                    });
                }
                Err(_) => {
                    task.abort();
                    let _ = task.await;
                    tracing::warn!(hook = %hook_name, "Post-hook timed out");
                    return serde_json::json!({
                        "error": format!(
                            "Post-hook {hook_name} timed out after {tool_name} executed"
                        )
                    });
                }
            };

            match action {
                HookAction::Allow => continue,
                HookAction::Transform { value } => {
                    result = value;
                }
                HookAction::Deny { .. } => {
                    // Post hooks can't deny — ignore
                    tracing::warn!(hook = %hook_name, "Post-hook returned Deny, ignoring");
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

    struct SlowPreHook {
        delay: Duration,
    }

    struct PanickingHook {
        phase: HookPhase,
    }

    #[async_trait::async_trait]
    impl ToolHook for PanickingHook {
        fn name(&self) -> &str {
            "panicking_hook"
        }

        fn phases(&self) -> Vec<HookPhase> {
            vec![self.phase]
        }

        async fn execute(&self, _ctx: &HookContext) -> HookAction {
            panic!("intentional hook panic")
        }
    }

    #[async_trait::async_trait]
    impl ToolHook for SlowPreHook {
        fn name(&self) -> &str {
            "slow_pre_hook"
        }

        fn phases(&self) -> Vec<HookPhase> {
            vec![HookPhase::Pre]
        }

        async fn execute(&self, _ctx: &HookContext) -> HookAction {
            tokio::time::sleep(self.delay).await;
            HookAction::Allow
        }
    }

    #[tokio::test]
    async fn empty_registry_allows() {
        let reg = HookRegistry::new();
        let (action, _) = reg
            .run_pre_hooks("echo", "agent-1", serde_json::json!({}))
            .await;
        assert!(matches!(action, HookAction::Allow));
    }

    #[tokio::test]
    async fn pre_hook_panic_is_contained_and_fails_closed() {
        let mut registry = HookRegistry::new();
        registry.register_global(Arc::new(PanickingHook {
            phase: HookPhase::Pre,
        }));

        let (action, arguments) = registry
            .run_pre_hooks("write_file", "agent-1", serde_json::json!({"path": "x"}))
            .await;

        assert_eq!(arguments, serde_json::json!({"path": "x"}));
        assert!(matches!(
            action,
            HookAction::Deny { reason }
                if reason.contains("panicked") && reason.contains("write_file")
        ));
    }

    #[tokio::test]
    async fn post_hook_panic_is_contained_as_failed_result() {
        let mut registry = HookRegistry::new();
        registry.register_global(Arc::new(PanickingHook {
            phase: HookPhase::Post,
        }));

        let result = registry
            .run_post_hooks("write_file", "agent-1", serde_json::json!({"ok": true}))
            .await;

        assert!(result["error"]
            .as_str()
            .is_some_and(|error| error.contains("panicked") && error.contains("write_file")));
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
    async fn pre_hook_timeout_fails_closed() {
        let mut reg = HookRegistry::with_config(HookConfig {
            timeout: Duration::from_millis(1),
            max_depth: 1,
        });
        reg.register_global(Arc::new(SlowPreHook {
            delay: Duration::from_secs(60),
        }));

        let (action, _) = reg
            .run_pre_hooks("external_side_effect", "agent-1", serde_json::json!({}))
            .await;

        let HookAction::Deny { reason } = action else {
            panic!("a timed-out pre-hook must deny tool execution");
        };
        assert!(reason.contains("slow_pre_hook"));
        assert!(reason.contains("external_side_effect"));
    }

    #[tokio::test]
    async fn delayed_pre_hook_decision_is_honored_before_its_deadline() {
        let mut reg = HookRegistry::with_config(HookConfig {
            timeout: Duration::from_millis(100),
            max_depth: 1,
        });
        reg.register_global(Arc::new(SlowPreHook {
            delay: Duration::from_millis(10),
        }));

        let (action, _) = reg
            .run_pre_hooks("external_side_effect", "agent-1", serde_json::json!({}))
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
