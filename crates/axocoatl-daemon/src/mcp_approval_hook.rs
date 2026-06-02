//! The runtime hook that gates MCP tool calls behind user approval.
//!
//! Registered globally on the `HookRegistry` at daemon bootstrap. For every
//! tool call we ask: is this an MCP tool? If yes, look up the persisted
//! decision; on miss, call the approval gate and emit a WS frame so the
//! dashboard can prompt the user. On `Allow` we let the call proceed; on
//! `Deny` we return a deny action with a clear reason.

use std::sync::Arc;

use axocoatl_mcp::approval::{
    ApprovalContext, ApprovalResolution, McpApprovalGate, PersistScope, SharedApprovalGate,
};
use axocoatl_mcp::permissions::{McpPermissionStore, PermissionDecision, PermissionRecord};
use axocoatl_mcp::registry::McpToolRegistry;
use axocoatl_tools::hooks::{HookAction, HookContext, HookPhase, ToolHook};
use tokio::sync::{broadcast, RwLock};

use crate::stream::StreamFrame;

pub struct McpApprovalHook {
    registry: Arc<RwLock<McpToolRegistry>>,
    permissions: Arc<RwLock<McpPermissionStore>>,
    gate: SharedApprovalGate,
    stream_bus: broadcast::Sender<StreamFrame>,
}

impl McpApprovalHook {
    pub fn new(
        registry: Arc<RwLock<McpToolRegistry>>,
        permissions: Arc<RwLock<McpPermissionStore>>,
        gate: SharedApprovalGate,
        stream_bus: broadcast::Sender<StreamFrame>,
    ) -> Self {
        Self {
            registry,
            permissions,
            gate,
            stream_bus,
        }
    }
}

#[async_trait::async_trait]
impl ToolHook for McpApprovalHook {
    fn name(&self) -> &str {
        "mcp_approval"
    }

    fn phases(&self) -> Vec<HookPhase> {
        vec![HookPhase::Pre]
    }

    async fn execute(&self, ctx: &HookContext) -> HookAction {
        // Resolve "is this an MCP tool, and which server owns it?" The
        // registry holds qualified names like `mcp__server__tool`; native
        // builtins won't appear there.
        let qualified = &ctx.tool_name;
        let server = {
            let reg = self.registry.read().await;
            reg.server_for_tool(qualified).map(|s| s.to_string())
        };
        let Some(server) = server else {
            return HookAction::Allow;
        };

        // Already-recorded decision wins; no prompt needed.
        {
            let perms = self.permissions.read().await;
            if let Some(decision) = perms.lookup(&ctx.agent_id, &server, qualified) {
                return match decision {
                    PermissionDecision::Allow => HookAction::Allow,
                    PermissionDecision::Deny => HookAction::Deny {
                        reason: format!(
                            "Tool {} on {} is denied for agent {} by a recorded permission",
                            qualified, server, ctx.agent_id
                        ),
                    },
                };
            }
        }

        // Build the approval context. Arguments preview is JSON-stringified
        // and truncated so a multi-megabyte payload doesn't bloat the WS frame.
        let original = {
            let reg = self.registry.read().await;
            reg.original_name(qualified)
                .unwrap_or(qualified)
                .to_string()
        };
        let mut args_preview = serde_json::to_string(&ctx.value).unwrap_or_default();
        const PREVIEW_CAP: usize = 2048;
        if args_preview.len() > PREVIEW_CAP {
            args_preview.truncate(PREVIEW_CAP);
            args_preview.push_str("…(truncated)");
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let approval_ctx = ApprovalContext {
            approval_id: McpApprovalGate::new_approval_id(),
            agent_id: ctx.agent_id.clone(),
            server: server.clone(),
            tool: qualified.clone(),
            tool_display: original,
            arguments_preview: args_preview,
            requested_at: now,
        };

        // Park on the gate; the closure emits the WS frame the dashboard
        // listens for. Note: the bus may have zero subscribers (no dashboard
        // connected) — `send` returns Err in that case, which we ignore. The
        // approval will timeout and Deny, which is the safe default.
        let bus = self.stream_bus.clone();
        let resolution = self
            .gate
            .request(approval_ctx.clone(), |c| {
                let _ = bus.send(StreamFrame::McpApprovalRequired {
                    approval_id: c.approval_id.clone(),
                    agent_id: c.agent_id.clone(),
                    server: c.server.clone(),
                    tool: c.tool.clone(),
                    tool_display: c.tool_display.clone(),
                    arguments_preview: c.arguments_preview.clone(),
                    requested_at: c.requested_at,
                });
            })
            .await;

        // Persist the decision according to the scope the user chose.
        self.persist_resolution(&approval_ctx, &resolution).await;
        // Notify any other connected dashboards that this approval has been
        // resolved so they can close any open modal.
        let _ = self.stream_bus.send(StreamFrame::McpApprovalResolved {
            approval_id: approval_ctx.approval_id.clone(),
            decision: match resolution.decision {
                PermissionDecision::Allow => "allow".into(),
                PermissionDecision::Deny => "deny".into(),
            },
        });

        match resolution.decision {
            PermissionDecision::Allow => HookAction::Allow,
            PermissionDecision::Deny => HookAction::Deny {
                reason: format!(
                    "User denied {} on {}",
                    approval_ctx.tool, approval_ctx.server
                ),
            },
        }
    }
}

impl McpApprovalHook {
    async fn persist_resolution(&self, ctx: &ApprovalContext, res: &ApprovalResolution) {
        let mut perms = self.permissions.write().await;
        let rec = match res.persist_scope {
            PersistScope::Once => return,
            PersistScope::ThisAgentThisTool => PermissionRecord {
                agent_id: Some(ctx.agent_id.clone()),
                server: ctx.server.clone(),
                tool: Some(ctx.tool.clone()),
                decision: res.decision,
                recorded_at: 0,
            },
            PersistScope::ThisAgentThisServer => PermissionRecord {
                agent_id: Some(ctx.agent_id.clone()),
                server: ctx.server.clone(),
                tool: None,
                decision: res.decision,
                recorded_at: 0,
            },
            PersistScope::AnyAgentThisServer => PermissionRecord {
                agent_id: None,
                server: ctx.server.clone(),
                tool: None,
                decision: res.decision,
                recorded_at: 0,
            },
        };
        if let Err(e) = perms.record(rec) {
            tracing::warn!(error = %e, "failed to persist MCP permission decision");
        }
    }
}
