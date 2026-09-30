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
#[derive(Clone)]
pub struct AgentRunControl {
    inner: Arc<RunControlInner>,
}

#[derive(Clone)]
struct RunControlInner {
    id: AgentRunId,
    cancelled: Arc<AtomicBool>,
    notify: Arc<tokio::sync::Notify>,
    ancestors: Vec<Arc<AtomicBool>>,
    execution_boundary: Option<Arc<dyn crate::execution_boundary::ToolExecutionBoundary>>,
    boundary_failure: Arc<std::sync::Mutex<Option<String>>>,
}

impl std::fmt::Debug for AgentRunControl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentRunControl")
            .field("id", &self.inner.id)
            .field("cancelled", &self.is_cancelled())
            .field(
                "has_execution_boundary",
                &self.inner.execution_boundary.is_some(),
            )
            .finish_non_exhaustive()
    }
}

impl AgentRunControl {
    pub fn new(id: AgentRunId) -> Self {
        Self {
            inner: Arc::new(RunControlInner {
                id,
                cancelled: Arc::new(AtomicBool::new(false)),
                notify: Arc::new(tokio::sync::Notify::new()),
                ancestors: Vec::new(),
                execution_boundary: None,
                boundary_failure: Arc::new(std::sync::Mutex::new(None)),
            }),
        }
    }

    pub fn id(&self) -> &AgentRunId {
        &self.inner.id
    }

    /// Install host authority for this one execution, never from AgentInput JSON.
    pub fn with_execution_boundary(
        mut self,
        boundary: Arc<dyn crate::execution_boundary::ToolExecutionBoundary>,
    ) -> Self {
        // Existing host handles keep their own boundary selection while sharing
        // the same cancellation and failure signals with this actor handle.
        Arc::make_mut(&mut self.inner).execution_boundary = Some(boundary);
        self
    }

    pub fn execution_boundary(
        &self,
    ) -> Option<&Arc<dyn crate::execution_boundary::ToolExecutionBoundary>> {
        self.inner.execution_boundary.as_ref()
    }

    /// A durable-boundary failure is terminal across this controlled hierarchy;
    /// it must never become ordinary failed-worker input to model synthesis.
    pub fn execution_boundary_failure(&self) -> Option<String> {
        match self.inner.boundary_failure.lock() {
            Ok(failure) => failure.clone(),
            Err(_) => Some("execution boundary failure state is unavailable".into()),
        }
    }

    pub(crate) fn fail_execution_boundary(&self, error: String) {
        if let Ok(mut failure) = self.inner.boundary_failure.lock() {
            failure.get_or_insert(error);
        }
        self.inner.cancelled.store(true, Ordering::Release);
        for ancestor in &self.inner.ancestors {
            ancestor.store(true, Ordering::Release);
        }
        self.inner.notify.notify_waiters();
    }

    /// Independent child cancellation, inheriting every ancestor's Stop. The
    /// boundary is deliberately absent until the host provisions this child.
    pub fn child(&self, id: AgentRunId) -> Self {
        let mut ancestors = self.inner.ancestors.clone();
        ancestors.push(self.inner.cancelled.clone());
        Self {
            inner: Arc::new(RunControlInner {
                id,
                cancelled: Arc::new(AtomicBool::new(false)),
                notify: self.inner.notify.clone(),
                ancestors,
                execution_boundary: None,
                boundary_failure: self.inner.boundary_failure.clone(),
            }),
        }
    }

    /// Request cancellation. Returns `true` only for the first request.
    pub fn cancel(&self) -> bool {
        if self.inner.cancelled.swap(true, Ordering::AcqRel) {
            false
        } else {
            self.inner.notify.notify_waiters();
            true
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::Acquire)
            || self
                .inner
                .ancestors
                .iter()
                .any(|flag| flag.load(Ordering::Acquire))
    }

    /// Resolve once cancellation has been requested.
    pub async fn cancelled(&self) {
        loop {
            let notified = self.inner.notify.notified();
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

    #[tokio::test]
    async fn child_stop_is_exact_and_ancestor_stop_reaches_descendants() {
        let parent = AgentRunControl::new(AgentRunId::new("parent"));
        let first = parent.child(AgentRunId::new("first"));
        let sibling = parent.child(AgentRunId::new("sibling"));
        let grandchild = sibling.child(AgentRunId::new("grandchild"));
        first.cancel();
        assert!(first.is_cancelled());
        assert!(!parent.is_cancelled());
        assert!(!sibling.is_cancelled());
        assert!(!grandchild.is_cancelled());
        let waiting = grandchild.clone();
        let waiter = tokio::spawn(async move { waiting.cancelled().await });
        parent.cancel();
        tokio::time::timeout(std::time::Duration::from_secs(1), waiter)
            .await
            .unwrap()
            .unwrap();
        assert!(sibling.is_cancelled());
        assert!(grandchild.is_cancelled());
    }
}
