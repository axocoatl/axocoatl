//! Opaque execution-bound approval callback. It supplies no permission policy;
//! the existing hook still decides whether a human response is required.
use super::HookContext;
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookApprovalResolution {
    Approved,
    Denied { reason: String },
}

#[async_trait::async_trait]
pub trait HookApprovalBoundary: Send + Sync {
    fn actor_scope(&self) -> Result<String, String>;
    /// The caller is an existing trusted approval hook. The boundary binds this
    /// context to the exact actor/native invocation and owns durable waiting.
    async fn request_human_approval(
        &self,
        context: &HookContext,
        display_request: serde_json::Value,
        timeout: Duration,
    ) -> Result<HookApprovalResolution, String>;
}

pub type SharedHookApprovalBoundary = Arc<dyn HookApprovalBoundary>;
