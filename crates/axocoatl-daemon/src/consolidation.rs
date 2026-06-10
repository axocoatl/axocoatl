//! Background "sleep-time" memory consolidation.
//!
//! Periodically asks idle agents to promote durable facts from their Tier-4
//! semantic memory into their curated core-memory blocks (and tidy them). The
//! *agent* decides whether it has been idle long enough — the LLM pass only runs
//! past `idle_threshold_secs` — so this loop merely polls and asks. It never
//! touches per-agent memory directly (it can't; that state is actor-private).
//! Mirrors `start_supervision` / `start_scheduler`.

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

            // Snapshot live agents + their actor refs under a short read lock.
            // (Workers aren't in the registry — they're coordinator-spawned — so
            // there's no worker filtering to do.)
            let agents: Vec<(AgentId, ActorRef<AgentMessage>)> = {
                let d = daemon.read().await;
                let ids = d.agent_registry.list_ids().await;
                let mut out = Vec::with_capacity(ids.len());
                for id in ids {
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
