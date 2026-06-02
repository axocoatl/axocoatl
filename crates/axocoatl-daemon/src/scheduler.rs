//! Lightweight interval scheduler. Fires configured workflows on a schedule.
//!
//! `every` accepts: `30s`, `5m`, `2h`, `1d`. (Cron-expression support deferred.)

use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::time::interval;

use axocoatl_config::ScheduleConfigYaml;

use crate::bootstrap::AxocoatlDaemon;

/// Live state of a scheduled workflow, exposed via /api/schedules.
#[derive(Debug, Clone)]
pub struct ScheduleState {
    pub config: ScheduleConfigYaml,
    pub interval_secs: u64,
    pub last_fired_unix: Option<u64>,
    pub last_outcome: Option<String>,
    pub run_count: u64,
}

impl ScheduleState {
    pub fn next_fire_unix(&self) -> Option<u64> {
        let base = self.last_fired_unix.unwrap_or_else(now_unix);
        Some(base + self.interval_secs)
    }
}

/// Parse "30s" / "5m" / "2h" / "1d" → seconds.
pub fn parse_interval(s: &str) -> Result<u64, String> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Err("empty interval".into());
    }
    let (num_str, unit) = trimmed.split_at(trimmed.len() - 1);
    let n: u64 = num_str
        .parse()
        .map_err(|_| format!("invalid number in '{trimmed}'"))?;
    let mult = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86400,
        _ => {
            return Err(format!(
                "unknown unit '{unit}' in '{trimmed}' — use s/m/h/d"
            ))
        }
    };
    Ok(n.saturating_mul(mult))
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Shared scheduler state — a Vec<Mutex<...>> would be heavier; we keep
/// the whole table behind one Mutex since updates are infrequent.
pub type ScheduleTable = Arc<Mutex<Vec<ScheduleState>>>;

/// Populate the given shared table with schedule states and spawn one
/// background tokio task per enabled schedule. Each task wakes on its own
/// interval and fires its workflow against the supplied daemon handle.
pub fn start_scheduler(
    table: ScheduleTable,
    daemon: Arc<tokio::sync::RwLock<AxocoatlDaemon>>,
    schedules: Vec<ScheduleConfigYaml>,
) {
    for cfg in schedules {
        let interval_secs = match parse_interval(&cfg.every) {
            Ok(s) if s > 0 => s,
            Ok(_) => {
                tracing::warn!(schedule = %cfg.id, "Schedule interval is 0, skipping");
                continue;
            }
            Err(e) => {
                tracing::warn!(schedule = %cfg.id, error = %e, "Bad schedule interval, skipping");
                continue;
            }
        };

        let initial = ScheduleState {
            config: cfg.clone(),
            interval_secs,
            last_fired_unix: None,
            last_outcome: None,
            run_count: 0,
        };
        table.lock().unwrap().push(initial);

        // Always spawn a watcher task — the actual fire is gated on the
        // live `enabled` flag in the table, so PATCH /api/schedules/{id}
        // takes effect immediately without restarting the daemon.
        let sched_id = cfg.id.clone();
        let daemon_for_task = daemon.clone();
        let table_for_task = table.clone();

        tokio::spawn(async move {
            // Initial offset — wait one interval before the first fire so the
            // dashboard doesn't see an immediate run on startup.
            let mut ticker = interval(Duration::from_secs(interval_secs));
            ticker.tick().await; // consume the initial tick (fires immediately by default)

            loop {
                ticker.tick().await;

                // Read the *live* enabled flag + the current workflow/input from the table.
                let live = {
                    let t = match table_for_task.lock() {
                        Ok(t) => t,
                        Err(_) => continue,
                    };
                    t.iter().find(|s| s.config.id == sched_id).map(|s| {
                        (
                            s.config.enabled,
                            s.config.workflow.clone(),
                            s.config.input.clone(),
                        )
                    })
                };
                let Some((enabled, workflow_id, input)) = live else {
                    continue;
                };
                if !enabled {
                    tracing::debug!(schedule = %sched_id, "Schedule disabled, skipping tick");
                    continue;
                }
                tracing::info!(schedule = %sched_id, workflow = %workflow_id, "Firing scheduled workflow");

                // Route through the unified executor. Every schedule projects
                // into automation id `sched:{id}` via Automation::from_legacy
                // — same nodes + edges as the referenced workflow, plus the
                // Schedule trigger metadata.
                let outcome = {
                    let d = daemon_for_task.read().await;
                    d.execute_automation(&format!("sched:{sched_id}"), &input)
                        .await
                };

                let summary = match outcome {
                    Ok(out) => format!(
                        "OK · {} agents · {} tokens",
                        out.completed_agents.len(),
                        out.total_token_usage.input_tokens + out.total_token_usage.output_tokens
                    ),
                    Err(e) => format!("FAIL · {e}"),
                };

                if let Ok(mut t) = table_for_task.lock() {
                    if let Some(state) = t.iter_mut().find(|s| s.config.id == sched_id) {
                        state.last_fired_unix = Some(now_unix());
                        state.last_outcome = Some(summary);
                        state.run_count += 1;
                    }
                }
            }
        });
    }
}
