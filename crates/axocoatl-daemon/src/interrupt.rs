//! HITL (human-in-the-loop) interrupts.
//!
//! When an `Interrupt` node fires inside an automation, the executor:
//!
//!   1. Generates a fresh `run_id` if the automation didn't supply one.
//!   2. Resolves the node's `input` (the message shown to the operator).
//!   3. Registers a `PendingInterrupt` in the daemon's `pending_interrupts`
//!      map, keyed by `{automation_id}:{run_id}:{node_id}`.
//!   4. Emits a `StreamFrame::Event { event_type: "Interrupted" }` for run
//!      observers; the pending-interrupt collection drives the app's rail and
//!      Settings surfaces.
//!   5. Awaits `notify.notified()` — blocks until the operator resumes.
//!
//! `POST /api/automations/{id}/runs/{run_id}/resume` atomically claims the
//! matching `PendingInterrupt`. A live executor is woken through its notifier.
//! After a daemon restart, bootstrap recreates the entry from the persisted
//! run and `interrupt_parked` checkpoint; resolving that entry starts a new
//! continuation from the saved outputs and active edges.

use std::sync::Arc;
use tokio::sync::Notify;

/// One pending HITL interrupt — exists from `park()` until `resume()`.
#[derive(Clone)]
pub struct PendingInterrupt {
    pub automation_id: String,
    pub run_id: String,
    pub node_id: String,
    /// Message rendered for the operator — the resolved Interrupt input.
    pub message: String,
    /// Optional structured payload (currently unused, reserved for
    /// passing context that's awkward to embed in `message`).
    pub payload: serde_json::Value,
    /// Walltime when the interrupt was created.
    pub created_at_unix: u64,
    /// Notifier the executor blocks on. Cloned into both the dashboard
    /// side (for resume) and the executor side (for await).
    pub notify: Arc<Notify>,
    /// Set by `resume()` before notifying; the executor reads this when
    /// it wakes.
    pub resume_value: Arc<tokio::sync::Mutex<Option<String>>>,
    /// Set to true by `cancel()` instead of `resume()`. The executor reads it
    /// when it wakes, emits a distinct "Cancelled" event, and proceeds with
    /// this node's output set to empty.
    pub cancelled: Arc<std::sync::atomic::AtomicBool>,
    /// `true` when bootstrap reconstructed this entry from a durable run.
    /// There is no process-local executor to notify in that case; resolution
    /// must start a checkpoint continuation instead.
    pub(crate) recovered: bool,
}

impl PendingInterrupt {
    pub fn key(&self) -> String {
        format!("{}:{}:{}", self.automation_id, self.run_id, self.node_id)
    }
}

/// Serialized view for the API. Strips notifiers and the resume mutex.
#[derive(serde::Serialize)]
pub struct PendingInterruptView<'a> {
    pub automation_id: &'a str,
    pub run_id: &'a str,
    pub node_id: &'a str,
    pub message: &'a str,
    pub payload: &'a serde_json::Value,
    pub created_at_unix: u64,
}

impl<'a> From<&'a PendingInterrupt> for PendingInterruptView<'a> {
    fn from(p: &'a PendingInterrupt) -> Self {
        Self {
            automation_id: &p.automation_id,
            run_id: &p.run_id,
            node_id: &p.node_id,
            message: &p.message,
            payload: &p.payload,
            created_at_unix: p.created_at_unix,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum InterruptResolutionError {
    #[error("no pending interrupt at {0}")]
    NotFound(String),
    #[error("cannot recover pending interrupt at {key}: {reason}")]
    Recovery { key: String, reason: String },
}
