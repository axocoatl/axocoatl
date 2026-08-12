//! Runtime gate that turns an MCP tool call without a recorded decision
//! into a human-in-the-loop approval prompt.
//!
//! The flow:
//! 1. A tool dispatch checks [`crate::permissions::McpPermissionStore::lookup`].
//! 2. If it returns `None`, the executor calls [`McpApprovalGate::request`].
//! 3. The gate generates a fresh `approval_id`, parks a oneshot sender, and
//!    fires the caller-supplied notifier (typically a WS `mcp-approval-required`
//!    frame so the dashboard pops a modal).
//! 4. The tool dispatch awaits the receiver. The user clicks Allow/Deny in
//!    the modal → an HTTP/WS handler resolves the approval and the
//!    dispatch resumes.
//!
//! The gate is provider-agnostic: it doesn't know about WebSocket frames
//! directly. The caller registering a request supplies a *notifier closure*
//! that knows how to surface the prompt (today that's "emit an
//! `mcp-approval-required` StreamFrame"). Decoupling lets tests drive the
//! gate without an HTTP server.

use crate::permissions::PermissionDecision;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{oneshot, Mutex};

/// Context the user needs to make an informed decision.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalContext {
    pub approval_id: String,
    pub agent_id: String,
    pub server: String,
    /// Qualified tool name (`mcp__server__tool`).
    pub tool: String,
    /// Original (un-qualified) tool name for display.
    pub tool_display: String,
    /// JSON-stringified arguments. Truncated to ~2 KB upstream so the WS
    /// frame doesn't carry megabytes.
    pub arguments_preview: String,
    pub requested_at: u64,
}

/// Resolution received from the UI.
#[derive(Debug, Clone)]
pub struct ApprovalResolution {
    pub decision: PermissionDecision,
    /// How to persist the decision (or not at all, for "Allow once").
    pub persist_scope: PersistScope,
}

/// Which scope the user picked when persisting an approval.
#[derive(Debug, Clone, Copy)]
pub enum PersistScope {
    /// Don't save — applies to this call only.
    Once,
    /// Save as `{agent_id, server, tool}` exact match.
    ThisAgentThisTool,
    /// Save as `{agent_id, server, tool: None}` — any tool from this server,
    /// only when called by this agent.
    ThisAgentThisServer,
    /// Save as `{agent_id: None, server, tool: None}` — most permissive.
    AnyAgentThisServer,
}

/// The gate. Held inside the daemon as `Arc<McpApprovalGate>` and consulted
/// by the tool-dispatch hook.
pub struct McpApprovalGate {
    pending: Mutex<HashMap<String, PendingApproval>>,
}

/// The sender that resumes a parked tool call plus the context a reconnecting
/// product surface needs to render that decision safely.
///
/// Keeping both under the gate's single mutex makes the gate authoritative:
/// an approval cannot be visible in a reconnect snapshot after it has already
/// been resolved, and a newly parked approval is snapshot-visible before its
/// live notification is emitted.
struct PendingApproval {
    context: ApprovalContext,
    sender: oneshot::Sender<ApprovalResolution>,
}

impl McpApprovalGate {
    pub fn new() -> Self {
        Self {
            pending: Mutex::new(HashMap::new()),
        }
    }

    /// Park a new pending approval. The closure `on_request` is called
    /// synchronously with the freshly-built `ApprovalContext` so the caller
    /// can surface it (typically by emitting a WS frame). The returned
    /// future resolves when the user clicks something in the UI or the
    /// timeout fires.
    ///
    /// **Timeout**: 5 minutes. Long enough for a human to come back from
    /// the kitchen, short enough that a forgotten approval doesn't pin
    /// daemon resources indefinitely. Default is `Deny` on timeout.
    pub async fn request<F>(&self, ctx: ApprovalContext, on_request: F) -> ApprovalResolution
    where
        F: FnOnce(&ApprovalContext),
    {
        let (tx, rx) = oneshot::channel::<ApprovalResolution>();
        {
            let mut pending = self.pending.lock().await;
            pending.insert(
                ctx.approval_id.clone(),
                PendingApproval {
                    context: ctx.clone(),
                    sender: tx,
                },
            );
        }
        on_request(&ctx);
        match tokio::time::timeout(Duration::from_secs(5 * 60), rx).await {
            Ok(Ok(res)) => res,
            Ok(Err(_)) | Err(_) => {
                // Receiver dropped or timed out — treat as a soft deny.
                // Clean up our pending entry if the timeout path got us here.
                let mut pending = self.pending.lock().await;
                pending.remove(&ctx.approval_id);
                ApprovalResolution {
                    decision: PermissionDecision::Deny,
                    persist_scope: PersistScope::Once,
                }
            }
        }
    }

    /// Resolve a pending approval. Returns `true` if a request was waiting
    /// for this id and `false` if the id was unknown (already resolved,
    /// or never existed).
    pub async fn resolve(&self, approval_id: &str, res: ApprovalResolution) -> bool {
        let mut pending = self.pending.lock().await;
        if let Some(pending) = pending.remove(approval_id) {
            // If the receiver hung up (rare), the send fails — that's fine,
            // the timeout path will Deny.
            let _ = pending.sender.send(res);
            true
        } else {
            false
        }
    }

    /// Snapshot of pending approvals — for the dashboard's "waiting" badge.
    pub async fn pending_ids(&self) -> Vec<String> {
        self.pending.lock().await.keys().cloned().collect()
    }

    /// Authoritative snapshot of every parked approval, including all of the
    /// context required to render and resolve it after a WebSocket reconnect.
    ///
    /// The order is stable so parallel requests do not randomly reshuffle in
    /// a FIFO approval surface on each reconnect.
    pub async fn pending_contexts(&self) -> Vec<ApprovalContext> {
        let mut contexts: Vec<_> = self
            .pending
            .lock()
            .await
            .values()
            .map(|pending| pending.context.clone())
            .collect();
        contexts.sort_by(|a, b| {
            a.requested_at
                .cmp(&b.requested_at)
                .then_with(|| a.approval_id.cmp(&b.approval_id))
        });
        contexts
    }

    /// Generate a stable id for a new approval request. Uses uuid v4 so
    /// the WS layer can carry it as an opaque string.
    pub fn new_approval_id() -> String {
        format!("appr-{}", uuid::Uuid::new_v4())
    }
}

impl Default for McpApprovalGate {
    fn default() -> Self {
        Self::new()
    }
}

/// Wrapper around a single Arc<gate> so the daemon and the WS handlers
/// share one instance.
pub type SharedApprovalGate = Arc<McpApprovalGate>;

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn allow_resolution_flows_through() {
        let gate = Arc::new(McpApprovalGate::new());
        let g = gate.clone();
        let ctx = ApprovalContext {
            approval_id: "id1".into(),
            agent_id: "a".into(),
            server: "fs".into(),
            tool: "mcp__fs__read".into(),
            tool_display: "read".into(),
            arguments_preview: "{}".into(),
            requested_at: 0,
        };
        let fut = tokio::spawn(async move {
            g.request(ctx, |_| { /* normally emits WS frame */ }).await
        });
        // Give the request a tick to register.
        tokio::task::yield_now().await;
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        assert!(
            gate.resolve(
                "id1",
                ApprovalResolution {
                    decision: PermissionDecision::Allow,
                    persist_scope: PersistScope::Once
                }
            )
            .await
        );
        let res = fut.await.unwrap();
        assert_eq!(res.decision, PermissionDecision::Allow);
    }

    #[tokio::test]
    async fn resolve_unknown_id_returns_false() {
        let gate = McpApprovalGate::new();
        let res = gate
            .resolve(
                "nope",
                ApprovalResolution {
                    decision: PermissionDecision::Allow,
                    persist_scope: PersistScope::Once,
                },
            )
            .await;
        assert!(!res);
    }

    #[tokio::test]
    async fn pending_context_snapshot_tracks_parallel_requests_and_resolution() {
        fn context(id: &str, requested_at: u64) -> ApprovalContext {
            ApprovalContext {
                approval_id: id.into(),
                agent_id: format!("agent-{id}"),
                server: "filesystem".into(),
                tool: "mcp__filesystem__write".into(),
                tool_display: "write".into(),
                arguments_preview: format!(r#"{{"id":"{id}"}}"#),
                requested_at,
            }
        }

        fn allow_once() -> ApprovalResolution {
            ApprovalResolution {
                decision: PermissionDecision::Allow,
                persist_scope: PersistScope::Once,
            }
        }

        let gate = Arc::new(McpApprovalGate::new());
        let (registered_tx, mut registered_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut requests = Vec::new();
        // Register in reverse timestamp/id order to prove snapshot order is
        // deterministic rather than HashMap iteration order.
        for ctx in [context("ap-2", 20), context("ap-1", 10)] {
            let gate = gate.clone();
            let registered_tx = registered_tx.clone();
            requests.push(tokio::spawn(async move {
                gate.request(ctx, |context| {
                    let _ = registered_tx.send(context.approval_id.clone());
                })
                .await
            }));
        }
        drop(registered_tx);
        registered_rx
            .recv()
            .await
            .expect("first request registered");
        registered_rx
            .recv()
            .await
            .expect("second request registered");

        let pending = gate.pending_contexts().await;
        assert_eq!(
            pending
                .iter()
                .map(|context| context.approval_id.as_str())
                .collect::<Vec<_>>(),
            ["ap-1", "ap-2"]
        );
        assert_eq!(pending[0].arguments_preview, r#"{"id":"ap-1"}"#);

        assert!(gate.resolve("ap-1", allow_once()).await);
        assert_eq!(
            gate.pending_contexts()
                .await
                .into_iter()
                .map(|context| context.approval_id)
                .collect::<Vec<_>>(),
            ["ap-2"]
        );
        assert!(gate.resolve("ap-2", allow_once()).await);
        assert!(gate.pending_contexts().await.is_empty());

        for request in requests {
            assert_eq!(
                request.await.expect("request task completes").decision,
                PermissionDecision::Allow
            );
        }
    }
}
