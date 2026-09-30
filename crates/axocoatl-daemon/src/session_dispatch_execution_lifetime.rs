//! Process-local execution ownership, separate from historical bindings and
//! read-only controller handles. A ticket ends only after its owning work drops.
use super::*;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

#[derive(Default)]
pub(super) struct ExecutionLifetimes {
    active: AtomicUsize,
    settlement_pending: AtomicBool,
    changed: tokio::sync::Notify,
}

impl ExecutionLifetimes {
    pub(super) fn only_driver_remains(&self, has_driver: bool) -> bool {
        self.active.load(Ordering::Acquire) <= usize::from(has_driver)
    }

    pub(super) fn is_idle(&self) -> bool {
        self.active.load(Ordering::Acquire) == 0
    }
}

/// Keep this as the LAST field of an owner, after its behavior, provider and
/// child tasks. Rust drops fields in declaration order; observing zero must not
/// race the destruction or accounting of any work retained by that owner.
pub(super) struct ExecutionTicket {
    _controller: SessionDispatchController,
    lifetimes: Arc<ExecutionLifetimes>,
    changed: Arc<tokio::sync::Notify>,
}

impl Drop for ExecutionTicket {
    fn drop(&mut self) {
        self.lifetimes.active.fetch_sub(1, Ordering::AcqRel);
        self.lifetimes
            .settlement_pending
            .store(true, Ordering::Release);
        self.lifetimes.changed.notify_waiters();
        self.changed.notify_waiters();
        self._controller.reconcile_released_tickets(&self.lifetimes);
    }
}

/// Every ordinary controller lock hands pending settlement to its successor
/// after releasing the gate. No task/runtime or next API request is required.
pub(super) struct DispatchGuard<'a> {
    controller: &'a SessionDispatchController,
    state: Option<MutexGuard<'a, DispatchState>>,
    lifetimes: Arc<ExecutionLifetimes>,
}
impl<'a> DispatchGuard<'a> {
    pub(super) fn new(
        controller: &'a SessionDispatchController,
        state: MutexGuard<'a, DispatchState>,
    ) -> Self {
        let lifetimes = state.execution_lifetimes.clone();
        Self {
            controller,
            state: Some(state),
            lifetimes,
        }
    }
}
impl std::ops::Deref for DispatchGuard<'_> {
    type Target = DispatchState;
    fn deref(&self) -> &DispatchState {
        self.state.as_deref().expect("held controller guard")
    }
}
impl std::ops::DerefMut for DispatchGuard<'_> {
    fn deref_mut(&mut self) -> &mut DispatchState {
        self.state.as_deref_mut().expect("held controller guard")
    }
}
impl Drop for DispatchGuard<'_> {
    fn drop(&mut self) {
        drop(self.state.take());
        // Inspect after unlock. If a ticket releases after this inspection,
        // its try_lock can acquire the free gate or hand off to its new owner.
        self.controller.reconcile_released_tickets(&self.lifetimes);
    }
}
impl SessionDispatchController {
    fn reconcile_released_tickets(&self, lifetimes: &ExecutionLifetimes) {
        while lifetimes.settlement_pending.load(Ordering::Acquire) {
            let Ok(mut state) = self.state.try_lock() else {
                // Do not clear the flag: that lock owner's post-release handoff
                // is now responsible. A poisoned owner remains fail closed.
                return;
            };
            if lifetimes.settlement_pending.swap(false, Ordering::AcqRel) {
                let result = (|| {
                    let snapshot = state.canonical.snapshot(&state.turn_id).map_err(error)?;
                    if snapshot.contract().stop_requested().is_some() {
                        state.reconcile_control_commands()?;
                    }
                    Ok(())
                })();
                let _ = state.fail_closed(result);
            }
            // This internal raw guard has the same post-unlock check as the
            // public guard. A release racing its final write cannot be lost.
            drop(state);
        }
    }
}

impl DispatchState {
    pub(super) fn execution_admission(&self) -> Result<()> {
        self.ready()?;
        if self.execution_admission_closed {
            return Err(error("Session lifecycle has closed execution admission"));
        }
        if self
            .canonical
            .snapshot(&self.turn_id)
            .map_err(error)?
            .contract()
            .stop_requested()
            .is_some()
        {
            return Err(error("turn finalization has closed execution admission"));
        }
        Ok(())
    }

    pub(super) fn acquire_execution_ticket(
        &self,
        controller: &SessionDispatchController,
    ) -> Result<ExecutionTicket> {
        self.execution_admission()?;
        self.acquire_settlement_ticket(controller)
    }

    /// Own an already allocated child while it checks whether work was stopped.
    /// This lifetime ticket grants no provider, tool, or checkpoint authority.
    pub(super) fn acquire_settlement_ticket(
        &self,
        controller: &SessionDispatchController,
    ) -> Result<ExecutionTicket> {
        self.ready()?;
        self.execution_lifetimes
            .active
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |active| {
                active.checked_add(1)
            })
            .map_err(|_| error("too many live Session executions"))?;
        Ok(ExecutionTicket {
            _controller: controller.clone(),
            lifetimes: self.execution_lifetimes.clone(),
            changed: self.changed.clone(),
        })
    }
}

impl SessionDispatchController {
    pub(crate) async fn wait_for_registered_executions(&self, timeout: Duration) -> Result<()> {
        let lifetimes = {
            let state = self.lock()?;
            if !state.execution_admission_closed {
                return Err(error(
                    "close execution admission before waiting for Session cleanup",
                ));
            }
            state.execution_lifetimes.clone()
        };
        tokio::time::timeout(timeout, async {
            loop {
                let notification = lifetimes.changed.notified();
                tokio::pin!(notification);
                notification.as_mut().enable();
                if lifetimes.active.load(Ordering::Acquire) == 0 {
                    return;
                }
                notification.await;
            }
        })
        .await
        .map_err(|_| error("Session executions still own pending work; cleanup ownership retained"))
    }
}
