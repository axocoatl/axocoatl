//! Cooperative control and truthful outcomes for one agent execution.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use axocoatl_core::AgentOutput;

/// Stable identity for one execution submitted to an agent actor.
///
/// Callers should persist this value with the owning session turn.  It is
/// deliberately caller-supplied rather than actor-generated so a reconnecting
/// client can address the same execution.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AgentRunId(Arc<str>);

impl AgentRunId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(Arc::from(id.into()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for AgentRunId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Cloneable cancellation handle shared by the caller and the actor behavior.
///
/// Cancellation is cooperative. Provider streams may be dropped immediately;
/// already-started tools are allowed to reach a safe boundary so callers are
/// never told that an external or filesystem side effect was rolled back.
#[derive(Debug, Clone)]
pub struct AgentRunControl {
    id: AgentRunId,
    cancelled: Arc<AtomicBool>,
    notify: Arc<tokio::sync::Notify>,
}

impl AgentRunControl {
    pub fn new(id: AgentRunId) -> Self {
        Self {
            id,
            cancelled: Arc::new(AtomicBool::new(false)),
            notify: Arc::new(tokio::sync::Notify::new()),
        }
    }

    pub fn id(&self) -> &AgentRunId {
        &self.id
    }

    /// Request cancellation. Returns `true` only for the first request.
    pub fn cancel(&self) -> bool {
        if self.cancelled.swap(true, Ordering::AcqRel) {
            false
        } else {
            self.notify.notify_waiters();
            true
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    /// Resolve once cancellation has been requested.
    pub async fn cancelled(&self) {
        loop {
            let notified = self.notify.notified();
            if self.is_cancelled() {
                return;
            }
            notified.await;
        }
    }
}

/// A completed execution or a cooperative cancellation with honest partial
/// output and usage accumulated up to the last safe boundary.
#[derive(Debug, Clone)]
pub enum AgentRunOutcome {
    Completed(AgentOutput),
    Cancelled {
        run_id: AgentRunId,
        partial_output: AgentOutput,
    },
}

impl AgentRunOutcome {
    pub fn output(&self) -> &AgentOutput {
        match self {
            Self::Completed(output) => output,
            Self::Cancelled { partial_output, .. } => partial_output,
        }
    }

    pub fn into_output(self) -> AgentOutput {
        match self {
            Self::Completed(output) => output,
            Self::Cancelled { partial_output, .. } => partial_output,
        }
    }

    pub fn is_cancelled(&self) -> bool {
        matches!(self, Self::Cancelled { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cancellation_is_idempotent_and_wakes_waiters() {
        let control = AgentRunControl::new(AgentRunId::new("turn-1"));
        let waiter_control = control.clone();
        let waiter = tokio::spawn(async move { waiter_control.cancelled().await });

        assert!(control.cancel());
        assert!(!control.cancel());
        waiter.await.unwrap();
        assert!(control.is_cancelled());
        assert_eq!(control.id().as_str(), "turn-1");
    }

    #[tokio::test]
    async fn already_cancelled_waiter_does_not_miss_notification() {
        let control = AgentRunControl::new(AgentRunId::new("turn-2"));
        control.cancel();
        tokio::time::timeout(std::time::Duration::from_millis(50), control.cancelled())
            .await
            .expect("an already-cancelled run should resolve immediately");
    }
}
