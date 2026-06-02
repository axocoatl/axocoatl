//! Proactive agents — agents that act on their own, with no user prompt.
//!
//! A proactive agent fires on a **trigger**: a fixed interval, or a named
//! lattice event. This is the autonomous half of "Always-On" — the other half
//! being the Always-On *Service* (`axocoatl-service`), which keeps the daemon
//! *process* running 24/7. Proactive agents make the agents *act* on their
//! own while that process runs.

use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::time::interval;

use axocoatl_config::{ProactiveConfigYaml, ProactiveTrigger};
use axocoatl_coordination::{EventLattice, EventType};

use crate::bootstrap::AxocoatlDaemon;
use crate::scheduler::parse_interval;

/// Minimum gap between two fires of an event-triggered proactive agent.
/// Guards against a self-loop where the agent emits the event it reacts to.
const EVENT_COOLDOWN_SECS: u64 = 30;

/// Live state of one proactive agent, exposed via `/api/proactive`.
#[derive(Debug, Clone)]
pub struct ProactiveState {
    pub config: ProactiveConfigYaml,
    pub last_fired_unix: Option<u64>,
    pub last_outcome: Option<String>,
    pub run_count: u64,
}

/// Shared table of proactive-agent state. One Mutex for the whole table —
/// updates are infrequent.
pub type ProactiveTable = Arc<Mutex<Vec<ProactiveState>>>;

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The canonical name of a lattice event, for matching against an
/// `OnEvent { event }` trigger. Custom events match on their own name.
fn event_name(et: &EventType) -> String {
    match et {
        EventType::TaskAvailable { .. } => "TaskAvailable".to_string(),
        EventType::TaskCompleted { .. } => "TaskCompleted".to_string(),
        EventType::AgentActivated { .. } => "AgentActivated".to_string(),
        EventType::AgentFailed { .. } => "AgentFailed".to_string(),
        EventType::ToolResult { .. } => "ToolResult".to_string(),
        EventType::UserInput => "UserInput".to_string(),
        EventType::WorkflowCompleted => "WorkflowCompleted".to_string(),
        EventType::Custom(s) => s.clone(),
    }
}

/// Register every proactive agent and spawn a background runner for each.
/// Schedule-triggered agents wake on their interval; event-triggered agents
/// subscribe to the lattice. Each runner reads the *live* `enabled` flag from
/// `table`, so toggling one takes effect without a daemon restart.
pub fn start_proactive_runners(
    table: ProactiveTable,
    daemon: Arc<tokio::sync::RwLock<AxocoatlDaemon>>,
    configs: Vec<ProactiveConfigYaml>,
    event_lattice: Arc<EventLattice>,
) {
    for cfg in configs {
        table.lock().unwrap().push(ProactiveState {
            config: cfg.clone(),
            last_fired_unix: None,
            last_outcome: None,
            run_count: 0,
        });

        match &cfg.trigger {
            ProactiveTrigger::Schedule { every } => {
                let secs = match parse_interval(every) {
                    Ok(s) if s > 0 => s,
                    _ => {
                        tracing::warn!(
                            proactive = %cfg.id,
                            "bad/zero schedule interval — skipping"
                        );
                        continue;
                    }
                };
                spawn_schedule_runner(cfg.id.clone(), secs, table.clone(), daemon.clone());
            }
            ProactiveTrigger::OnEvent { event } => {
                spawn_event_runner(
                    cfg.id.clone(),
                    event.clone(),
                    table.clone(),
                    daemon.clone(),
                    event_lattice.subscribe(),
                );
            }
        }
    }
}

/// Look up a proactive agent's live `(enabled, agent, input, last_fired)`.
fn live_state(table: &ProactiveTable, id: &str) -> Option<(bool, String, String, Option<u64>)> {
    let t = table.lock().ok()?;
    t.iter().find(|s| s.config.id == id).map(|s| {
        (
            s.config.enabled,
            s.config.agent.clone(),
            s.config.input.clone(),
            s.last_fired_unix,
        )
    })
}

/// Fire a proactive agent once and record the outcome in the table.
async fn fire(
    id: &str,
    agent: &str,
    input: &str,
    table: &ProactiveTable,
    daemon: &Arc<tokio::sync::RwLock<AxocoatlDaemon>>,
) {
    tracing::info!(proactive = %id, agent = %agent, "proactive agent firing");
    // Route through the unified automation executor — every proactive
    // projects into automation id `pro:{id}`. Single-node today, but the
    // visual editor can grow them into multi-node graphs.
    let outcome = {
        let d = daemon.read().await;
        d.execute_automation(&format!("pro:{id}"), input).await
    };
    let summary = match outcome {
        Ok(out) => format!(
            "OK · {} agents · {} tokens",
            out.completed_agents.len(),
            out.total_token_usage.input_tokens + out.total_token_usage.output_tokens
        ),
        Err(e) => format!("FAIL · {e}"),
    };
    // `agent` was only used to log + spawn; keep it as a log breadcrumb.
    let _ = agent;
    if let Ok(mut t) = table.lock() {
        if let Some(state) = t.iter_mut().find(|s| s.config.id == id) {
            state.last_fired_unix = Some(now_unix());
            state.last_outcome = Some(summary);
            state.run_count += 1;
        }
    }
}

/// Interval-triggered runner — wakes every `interval_secs` and fires if the
/// agent is still enabled.
fn spawn_schedule_runner(
    id: String,
    interval_secs: u64,
    table: ProactiveTable,
    daemon: Arc<tokio::sync::RwLock<AxocoatlDaemon>>,
) {
    tokio::spawn(async move {
        let mut ticker = interval(Duration::from_secs(interval_secs));
        ticker.tick().await; // consume the immediate first tick
        loop {
            ticker.tick().await;
            let Some((enabled, agent, input, _)) = live_state(&table, &id) else {
                continue;
            };
            if !enabled {
                continue;
            }
            fire(&id, &agent, &input, &table, &daemon).await;
        }
    });
}

/// Event-triggered runner — fires when a matching lattice event arrives, with
/// a cooldown so an agent that emits the event it reacts to can't self-loop.
fn spawn_event_runner(
    id: String,
    target_event: String,
    table: ProactiveTable,
    daemon: Arc<tokio::sync::RwLock<AxocoatlDaemon>>,
    mut events: tokio::sync::broadcast::Receiver<axocoatl_coordination::EventNotification>,
) {
    use tokio::sync::broadcast::error::RecvError;
    tokio::spawn(async move {
        loop {
            match events.recv().await {
                Ok(notif) => {
                    if event_name(&notif.event_type) != target_event {
                        continue;
                    }
                    let Some((enabled, agent, input, last_fired)) = live_state(&table, &id) else {
                        continue;
                    };
                    if !enabled {
                        continue;
                    }
                    // Cooldown — never react faster than once per window.
                    if let Some(last) = last_fired {
                        if now_unix().saturating_sub(last) < EVENT_COOLDOWN_SECS {
                            tracing::debug!(
                                proactive = %id,
                                "within cooldown — skipping event"
                            );
                            continue;
                        }
                    }
                    fire(&id, &agent, &input, &table, &daemon).await;
                }
                // A burst of events can lag a slow runner — skip the gap,
                // never let the runner die.
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => break,
            }
        }
    });
}
