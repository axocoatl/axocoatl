//! Background "sleep-time" memory consolidation.
//!
//! Periodically asks idle agents to promote durable facts from their Tier-4
//! semantic memory into their curated core-memory blocks (and tidy them). The
//! *agent* decides whether it has been idle long enough — the LLM pass only runs
//! past `idle_threshold_secs` — so this loop merely polls and asks. It never
//! touches per-agent memory directly (it can't; that state is actor-private).
//! Mirrors agent supervision and the canonical Automation trigger runtime.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ractor::ActorRef;
use tokio::sync::RwLock;

use axocoatl_actor::{consolidate_agent, AgentMessage};
use axocoatl_config::ConsolidationConfigYaml;
use axocoatl_core::AgentId;

use crate::bootstrap::AxocoatlDaemon;

/// How often the loop wakes to consider agents. The per-agent cadence is the
/// configured `interval_secs`, enforced via the `last_consolidated` map.
const POLL_INTERVAL: Duration = Duration::from_secs(60);

/// Whether the default background pass may ask this durable actor to
/// consolidate memory.
///
/// `#` identifies a runtime activation (including unresolved Ways/attempt
/// actors), while `:worker:` identifies coordinator-owned workers. Neither has
/// independent background authority. Ordinary Session actors use
/// `{session_id}:{agent_id}` and intentionally remain eligible.
fn is_default_consolidation_candidate(id: &AgentId) -> bool {
    !id.0.contains('#') && !id.0.contains(":worker:")
}

/// Spawn the consolidation loop. Returns immediately; runs until the process
/// exits. No-op when disabled.
pub fn start_consolidation(daemon: Arc<RwLock<AxocoatlDaemon>>, config: ConsolidationConfigYaml) {
    if !config.enabled {
        return;
    }
    tokio::spawn(async move {
        let mut last_consolidated: HashMap<AgentId, Instant> = HashMap::new();
        let min_interval = Duration::from_secs(config.interval_secs);

        loop {
            tokio::time::sleep(POLL_INTERVAL).await;

            // Snapshot eligible durable agents + their actor refs under a short
            // read lock. Runtime activations and coordinator Workers can share
            // this registry transiently, but never own background work.
            let agents: Vec<(AgentId, ActorRef<AgentMessage>)> = {
                let d = daemon.read().await;
                let ids = d.agent_registry.list_ids().await;
                let mut out = Vec::with_capacity(ids.len());
                for id in ids {
                    if !is_default_consolidation_candidate(&id) {
                        continue;
                    }
                    if let Some(actor) = d.agent_registry.get(&id).await {
                        out.push((id, actor));
                    }
                }
                out
            };

            let now = Instant::now();
            for (id, actor) in agents {
                if let Some(t) = last_consolidated.get(&id) {
                    if now.duration_since(*t) < min_interval {
                        continue; // consolidated recently
                    }
                }
                match consolidate_agent(&actor, config.idle_threshold_secs).await {
                    // The pass ran (idle long enough) — record it so we honor the
                    // interval, even if it made no edits.
                    Ok(report) if !report.skipped => {
                        last_consolidated.insert(id.clone(), Instant::now());
                        if !report.blocks_touched.is_empty() {
                            tracing::info!(
                                agent = %id,
                                promoted = report.promoted,
                                rewritten = report.rewritten,
                                tokens = report.tokens_used,
                                "background consolidation"
                            );
                        }
                    }
                    // Skipped — not idle long enough (or no memory). Retry next poll.
                    Ok(_) => {}
                    Err(e) => {
                        tracing::debug!(agent = %id, error = %e, "consolidation request failed")
                    }
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_consolidation_excludes_runtime_activations_and_coordinator_workers() {
        for id in [
            "coder#1",
            "session-a:coder#2",
            "session-a#attempt-recovery",
            "session-a:lead:worker:tester",
            "lead:worker:researcher",
        ] {
            assert!(
                !is_default_consolidation_candidate(&AgentId::new(id)),
                "{id} must not receive default background consolidation"
            );
        }
    }

    #[test]
    fn default_consolidation_keeps_top_level_and_durable_session_actors() {
        for id in [
            "coder",
            "lead",
            "session-a:coder",
            "session-a:lead",
            "worker",
            "session-a:workerish:tester",
        ] {
            assert!(
                is_default_consolidation_candidate(&AgentId::new(id)),
                "{id} remains eligible for default background consolidation"
            );
        }
    }
}
