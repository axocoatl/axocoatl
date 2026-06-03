//! Agent supervision: restart crashed agents from their last checkpoint.
//!
//! Top-level agent actors are spawned without a ractor supervisor, so a crash
//! (panic or error in an actor's loop) would otherwise leave the agent down
//! until a manual `restart_agent`. This background runner polls agent liveness
//! and restarts any that have stopped unexpectedly — the new actor's `on_start`
//! restores its session, token usage, and behaviour state from the latest
//! checkpoint. A per-agent restart cap prevents a crash-looping agent from
//! restarting forever.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::RwLock;

use crate::bootstrap::AxocoatlDaemon;

/// How often to check agent liveness.
const POLL_INTERVAL: Duration = Duration::from_secs(5);
/// Maximum consecutive restarts before giving up on an agent (reset once it
/// stays healthy across a poll).
const MAX_RESTARTS: u32 = 5;

/// Spawn the supervision loop. Returns immediately; the loop runs until the
/// process exits. Mirrors `start_scheduler` / `start_proactive_runners`.
pub fn start_supervision(daemon: Arc<RwLock<AxocoatlDaemon>>) {
    tokio::spawn(async move {
        // agent_id -> consecutive restart attempts
        let mut attempts: HashMap<String, u32> = HashMap::new();

        loop {
            tokio::time::sleep(POLL_INTERVAL).await;

            // Snapshot the dead agents under a short read lock, then release it
            // before doing any restarts.
            let dead = {
                let d = daemon.read().await;
                d.dead_agents().await
            };

            // Reset counters for agents that are healthy again.
            attempts.retain(|id, _| dead.iter().any(|d| d == id));

            for id in dead {
                let n = attempts.entry(id.clone()).or_insert(0);
                if *n >= MAX_RESTARTS {
                    continue; // already gave up; logged below on the transition
                }
                *n += 1;
                let attempt = *n;

                tracing::warn!(
                    agent = %id,
                    attempt,
                    max = MAX_RESTARTS,
                    "agent is not running — restarting from last checkpoint"
                );

                let result = {
                    let d = daemon.read().await;
                    d.restart_agent(&id).await
                };
                match result {
                    Ok(()) => tracing::info!(agent = %id, attempt, "agent restarted by supervisor"),
                    Err(e) => {
                        tracing::error!(agent = %id, error = %e, "supervised restart failed")
                    }
                }

                if attempt >= MAX_RESTARTS {
                    tracing::error!(
                        agent = %id,
                        "agent reached the restart cap — leaving it down until config reload"
                    );
                }
            }
        }
    });
}
