//! HITL (human-in-the-loop) interrupts.
//!
//! When an `Interrupt` node fires inside an automation, the executor:
//!
//!   1. Generates a fresh `run_id` if the automation didn't supply one.
//!   2. Resolves the node's `input` (the message shown to the operator).
//!   3. Registers a `PendingInterrupt` in the daemon's `pending_interrupts`
//!      map, keyed by `{automation_id}:{run_id}:{node_id}`.
//!   4. Emits a `StreamFrame::Event { event_type: "Interrupted" }` so the
//!      dashboard's activity feed lights up.
//!   5. Awaits `notify.notified()` — blocks until the operator resumes.
//!
//! `POST /api/automations/{id}/runs/{run_id}/resume` writes the resume value
//! into the matching `PendingInterrupt`, removes it from the map, and
//! signals the notify. The executor wakes and continues with the resumed
//! value as this node's output (or appended, per `ResumeStrategy`).
//!
//! Run IDs are random per execution — caller wires them through. We don't
//! persist them across daemon restarts in v0.1; a restart cancels any
//! pending interrupts. That's a known limitation paired with the
//! "checkpoint store" follow-up which would make resume durable.

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
    /// Set to true by `cancel()` instead of `resume()` — executor
    /// treats this node's output as empty and proceeds.
    pub cancelled: Arc<std::sync::atomic::AtomicBool>,
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
