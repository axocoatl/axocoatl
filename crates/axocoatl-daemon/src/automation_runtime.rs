//! Live trigger runtime for canonical [`axocoatl_config::Automation`] records.
//!
//! One dispatcher watches `AutomationStore` and the event lattice. It does not
//! create a task per Automation: schedules are planned by one timer and events
//! are matched by one subscriber. Store edits are therefore observed live and
//! deleted or changed records cannot leave stale runners behind.
//! Event-triggered runs are single-flight and cooled down from both dispatch
//! and completion. That is the self-loop boundary: Skill events identify their
//! Skill as `produced_by`, not the Automation whose agent fired that Skill.
//!
//! Legacy YAML never enters this module. Bootstrap may seed `AutomationStore`
//! from YAML on first boot, after which the persisted store is the only source
//! of trigger configuration.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axocoatl_config::{
    Automation, AutomationNodeKind, AutomationTrigger, ProactiveConfigYaml, ProactiveTrigger,
    ScheduleConfigYaml,
};
use axocoatl_coordination::EventNotification;

use crate::bootstrap::AxocoatlDaemon;
use crate::error::DaemonError;
use crate::proactive::{ProactiveState, ProactiveTable};
use crate::scheduler::{parse_interval, ScheduleState, ScheduleTable};
use crate::workflow::WorkflowOutput;

const STORE_SCAN_INTERVAL: Duration = Duration::from_millis(250);
const EVENT_COOLDOWN_SECS: u64 = 30;

#[derive(Debug, Clone, PartialEq, Eq)]
enum TriggerReason {
    Schedule {
        every: String,
    },
    Event {
        event: String,
        payload: serde_json::Value,
    },
    Skill {
        skill_id: String,
        payload: serde_json::Value,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingFire {
    automation_id: String,
    reason: TriggerReason,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ScheduleCursor {
    every: String,
    enabled: bool,
    next_fire_unix: u64,
}

#[derive(Default)]
struct SchedulePlanner {
    cursors: HashMap<String, ScheduleCursor>,
}

impl SchedulePlanner {
    /// Reconcile the current store snapshot and return the runs due now.
    ///
    /// Cadence/enable changes reset the countdown. A due tick advances even
    /// while the previous run is active, so long executions skip overlap
    /// instead of building a burst backlog.
    fn reconcile(
        &mut self,
        automations: &[Automation],
        active: &HashSet<String>,
        now: u64,
    ) -> Vec<PendingFire> {
        let mut scheduled_ids = HashSet::new();
        let mut due = Vec::new();

        for automation in automations {
            let AutomationTrigger::Schedule { every, .. } = &automation.trigger else {
                continue;
            };
            scheduled_ids.insert(automation.id.clone());

            let Ok(interval_secs) = parse_interval(every) else {
                self.cursors.remove(&automation.id);
                continue;
            };
            if interval_secs == 0 {
                self.cursors.remove(&automation.id);
                continue;
            }

            let cursor = self
                .cursors
                .entry(automation.id.clone())
                .or_insert_with(|| ScheduleCursor {
                    every: every.clone(),
                    enabled: automation.enabled,
                    next_fire_unix: now.saturating_add(interval_secs),
                });

            if cursor.every != *every || cursor.enabled != automation.enabled {
                *cursor = ScheduleCursor {
                    every: every.clone(),
                    enabled: automation.enabled,
                    next_fire_unix: now.saturating_add(interval_secs),
                };
            }

            if automation.enabled && now >= cursor.next_fire_unix {
                cursor.next_fire_unix = now.saturating_add(interval_secs);
                if !active.contains(&automation.id) {
                    due.push(PendingFire {
                        automation_id: automation.id.clone(),
                        reason: TriggerReason::Schedule {
                            every: every.clone(),
                        },
                    });
                }
            }
        }

        self.cursors
            .retain(|automation_id, _| scheduled_ids.contains(automation_id));
        due
    }

    fn next_fire_unix(&self, automation_id: &str) -> Option<u64> {
        self.cursors.get(automation_id).map(|c| c.next_fire_unix)
    }
}

/// A short synchronous lease prevents overlapping runs for one Automation.
/// The lease removes itself even if its spawned task unwinds.
struct ActiveLease {
    automation_id: String,
    active: Arc<Mutex<HashSet<String>>>,
    completion_cooldown: Option<Arc<Mutex<HashMap<String, u64>>>>,
}

impl ActiveLease {
    fn acquire(
        automation_id: &str,
        active: Arc<Mutex<HashSet<String>>>,
        completion_cooldown: Option<Arc<Mutex<HashMap<String, u64>>>>,
    ) -> Option<Self> {
        let mut ids = active.lock().ok()?;
        if !ids.insert(automation_id.to_string()) {
            return None;
        }
        drop(ids);
        Some(Self {
            automation_id: automation_id.to_string(),
            active,
            completion_cooldown,
        })
    }
}

impl Drop for ActiveLease {
    fn drop(&mut self) {
        if let Ok(mut ids) = self.active.lock() {
            ids.remove(&self.automation_id);
        }
        // Event/Skill runs extend their cooldown from completion as well as
        // dispatch. This catches a self-produced event that sat in the
        // broadcast queue during a run longer than the cooldown window.
        if let Some(cooldowns) = &self.completion_cooldown {
            if let Ok(mut cooldowns) = cooldowns.lock() {
                cooldowns.insert(self.automation_id.clone(), now_unix());
            }
        }
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn active_snapshot(active: &Arc<Mutex<HashSet<String>>>) -> HashSet<String> {
    active.lock().map(|ids| ids.clone()).unwrap_or_default()
}

fn first_agent_id(automation: &Automation) -> String {
    automation
        .nodes
        .iter()
        .find_map(|node| match &node.kind {
            AutomationNodeKind::Agent { agent_id, .. } => Some(agent_id.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

fn schedule_compat_id(id: &str) -> String {
    id.strip_prefix("sched:").unwrap_or(id).to_string()
}

fn proactive_compat_id(id: &str) -> String {
    id.strip_prefix("pro:").unwrap_or(id).to_string()
}

/// Resolve an event payload to trigger text. Explicit `input`/`content`
/// fields win; a configured fallback wins over opaque JSON; otherwise the
/// payload itself remains available as compact JSON.
fn event_input(payload: &serde_json::Value, fallback: Option<&str>) -> String {
    payload
        .get("input")
        .and_then(serde_json::Value::as_str)
        .or_else(|| payload.get("content").and_then(serde_json::Value::as_str))
        .or_else(|| payload.as_str())
        .map(ToOwned::to_owned)
        .or_else(|| fallback.map(ToOwned::to_owned))
        .unwrap_or_else(|| {
            if payload.is_null() {
                String::new()
            } else {
                payload.to_string()
            }
        })
}

fn event_matches(
    automations: &[Automation],
    notification: &EventNotification,
    active: &HashSet<String>,
    cooldowns: &mut HashMap<String, u64>,
    now: u64,
) -> Vec<PendingFire> {
    let mut fires = Vec::new();
    for automation in automations {
        if !automation.enabled || active.contains(&automation.id) {
            continue;
        }
        // Automation-originated Skill events identify the Skill as producer,
        // not the Automation. The real self-loop guards are the active lease
        // plus cooldown at both dispatch and completion.
        if cooldowns
            .get(&automation.id)
            .is_some_and(|last| now.saturating_sub(*last) < EVENT_COOLDOWN_SECS)
        {
            continue;
        }

        let reason = match &automation.trigger {
            AutomationTrigger::OnEvent { event, .. } if notification.event_type.name() == event => {
                Some(TriggerReason::Event {
                    event: event.clone(),
                    payload: notification.payload.clone(),
                })
            }
            AutomationTrigger::OnSkill { skill_id }
                if notification.produced_by == format!("skill:{skill_id}") =>
            {
                Some(TriggerReason::Skill {
                    skill_id: skill_id.clone(),
                    payload: notification.payload.clone(),
                })
            }
            _ => None,
        };
        if let Some(reason) = reason {
            cooldowns.insert(automation.id.clone(), now);
            fires.push(PendingFire {
                automation_id: automation.id.clone(),
                reason,
            });
        }
    }
    fires
}

/// Re-check a planned fire against the live store snapshot immediately before
/// execution. This closes the scan-to-spawn race for delete, disable, cadence,
/// event, and Skill edits, and resolves input from the latest record.
fn input_if_still_current(automation: &Automation, reason: &TriggerReason) -> Option<String> {
    if !automation.enabled {
        return None;
    }
    match (&automation.trigger, reason) {
        (
            AutomationTrigger::Schedule { every, input },
            TriggerReason::Schedule {
                every: expected_every,
            },
        ) if every == expected_every => Some(input.clone().unwrap_or_default()),
        (
            AutomationTrigger::OnEvent { event, input },
            TriggerReason::Event {
                event: expected_event,
                payload,
            },
        ) if event == expected_event => Some(event_input(payload, input.as_deref())),
        (
            AutomationTrigger::OnSkill { skill_id },
            TriggerReason::Skill {
                skill_id: expected_skill,
                payload,
            },
        ) if skill_id == expected_skill => Some(event_input(payload, None)),
        _ => None,
    }
}

fn schedule_state(automation: &Automation, next_fire_unix: Option<u64>) -> ScheduleState {
    let AutomationTrigger::Schedule { every, input } = &automation.trigger else {
        unreachable!("schedule_state called for a non-schedule Automation")
    };
    let parsed = parse_interval(every);
    let interval_secs = parsed.as_ref().copied().unwrap_or(0);
    let invalid = match parsed {
        Ok(0) => Some("invalid cadence: interval must be greater than zero".to_string()),
        Ok(_) => None,
        Err(error) => Some(format!("invalid cadence: {error}")),
    };
    ScheduleState {
        automation_id: automation.id.clone(),
        config: ScheduleConfigYaml {
            id: schedule_compat_id(&automation.id),
            name: automation.name.clone(),
            workflow: automation.id.clone(),
            every: every.clone(),
            input: input.clone().unwrap_or_default(),
            enabled: automation.enabled,
        },
        interval_secs,
        next_fire_unix: if automation.enabled && invalid.is_none() {
            next_fire_unix
        } else {
            None
        },
        last_fired_unix: None,
        last_outcome: None,
        last_error: invalid,
        run_count: 0,
    }
}

fn proactive_state(automation: &Automation) -> ProactiveState {
    let (trigger_kind, trigger_detail, input, trigger) = match &automation.trigger {
        AutomationTrigger::OnEvent { event, input } => (
            "event".to_string(),
            event.clone(),
            input.clone().unwrap_or_default(),
            ProactiveTrigger::OnEvent {
                event: event.clone(),
            },
        ),
        AutomationTrigger::OnSkill { skill_id } => (
            "skill".to_string(),
            skill_id.clone(),
            String::new(),
            ProactiveTrigger::OnEvent {
                event: format!("skill:{skill_id}"),
            },
        ),
        _ => unreachable!("proactive_state called for a non-event Automation"),
    };
    ProactiveState {
        automation_id: automation.id.clone(),
        config: ProactiveConfigYaml {
            id: proactive_compat_id(&automation.id),
            name: automation.name.clone(),
            agent: first_agent_id(automation),
            trigger,
            input,
            enabled: automation.enabled,
        },
        trigger_kind,
        trigger_detail,
        last_fired_unix: None,
        last_outcome: None,
        last_error: None,
        run_count: 0,
    }
}

fn reconcile_observations(
    automations: &[Automation],
    planner: &SchedulePlanner,
    schedule_table: &ScheduleTable,
    proactive_table: &ProactiveTable,
) {
    if let Ok(mut table) = schedule_table.lock() {
        let old: HashMap<String, ScheduleState> = table
            .drain(..)
            .map(|state| (state.automation_id.clone(), state))
            .collect();
        let mut next = Vec::new();
        for automation in automations {
            if !matches!(&automation.trigger, AutomationTrigger::Schedule { .. }) {
                continue;
            }
            let mut state = schedule_state(automation, planner.next_fire_unix(&automation.id));
            if let Some(previous) = old.get(&automation.id) {
                state.last_fired_unix = previous.last_fired_unix;
                state.last_outcome = previous.last_outcome.clone();
                if state.last_error.is_none() && state.config.every == previous.config.every {
                    state.last_error = previous.last_error.clone();
                }
                state.run_count = previous.run_count;
            }
            next.push(state);
        }
        next.sort_by(|a, b| a.automation_id.cmp(&b.automation_id));
        *table = next;
    }

    if let Ok(mut table) = proactive_table.lock() {
        let old: HashMap<String, ProactiveState> = table
            .drain(..)
            .map(|state| (state.automation_id.clone(), state))
            .collect();
        let mut next = Vec::new();
        for automation in automations {
            if !matches!(
                &automation.trigger,
                AutomationTrigger::OnEvent { .. } | AutomationTrigger::OnSkill { .. }
            ) {
                continue;
            }
            let mut state = proactive_state(automation);
            if let Some(previous) = old.get(&automation.id) {
                state.last_fired_unix = previous.last_fired_unix;
                state.last_outcome = previous.last_outcome.clone();
                state.last_error = previous.last_error.clone();
                state.run_count = previous.run_count;
            }
            next.push(state);
        }
        next.sort_by(|a, b| a.automation_id.cmp(&b.automation_id));
        *table = next;
    }
}

fn outcome_summary(result: &Result<WorkflowOutput, DaemonError>) -> (String, Option<String>) {
    match result {
        Ok(output) if !output.failed_agents.is_empty() => {
            let detail = output
                .failed_agents
                .iter()
                .map(|(subject, error)| format!("{subject}: {error}"))
                .collect::<Vec<_>>()
                .join("; ");
            (
                format!("FAIL · {} failed step(s)", output.failed_agents.len()),
                Some(detail),
            )
        }
        Ok(output) => (
            format!(
                "OK · {} agents · {} tokens",
                output.completed_agents.len(),
                output.total_token_usage.input_tokens + output.total_token_usage.output_tokens
            ),
            None,
        ),
        Err(error) => (format!("FAIL · {error}"), Some(error.to_string())),
    }
}

/// Record a manual or automatic execution in the compatibility observation
/// caches. Configuration is copied from the supplied canonical record; the
/// tables never feed execution decisions back into the runtime.
pub fn record_automation_outcome(
    context: &crate::automation_executor::AutomationExecutionContext,
    automation: &Automation,
    result: &Result<WorkflowOutput, DaemonError>,
) {
    let (summary, error) = outcome_summary(result);
    let fired_at = now_unix();
    match &automation.trigger {
        AutomationTrigger::Schedule { .. } => {
            if let Ok(mut table) = context.schedule_table.lock() {
                if !table
                    .iter()
                    .any(|state| state.automation_id == automation.id)
                {
                    table.push(schedule_state(automation, None));
                }
                if let Some(state) = table
                    .iter_mut()
                    .find(|state| state.automation_id == automation.id)
                {
                    state.last_fired_unix = Some(fired_at);
                    state.last_outcome = Some(summary);
                    state.last_error = error;
                    state.run_count = state.run_count.saturating_add(1);
                }
            }
        }
        AutomationTrigger::OnEvent { .. } | AutomationTrigger::OnSkill { .. } => {
            if let Ok(mut table) = context.proactive_table.lock() {
                if !table
                    .iter()
                    .any(|state| state.automation_id == automation.id)
                {
                    table.push(proactive_state(automation));
                }
                if let Some(state) = table
                    .iter_mut()
                    .find(|state| state.automation_id == automation.id)
                {
                    state.last_fired_unix = Some(fired_at);
                    state.last_outcome = Some(summary);
                    state.last_error = error;
                    state.run_count = state.run_count.saturating_add(1);
                }
            }
        }
        AutomationTrigger::Manual => {}
    }
}

fn dispatch(
    context: crate::automation_executor::AutomationExecutionContext,
    fire: PendingFire,
    active: Arc<Mutex<HashSet<String>>>,
    cooldowns: Arc<Mutex<HashMap<String, u64>>>,
) {
    let completion_cooldown = matches!(
        &fire.reason,
        TriggerReason::Event { .. } | TriggerReason::Skill { .. }
    )
    .then_some(cooldowns);
    let Some(lease) = ActiveLease::acquire(&fire.automation_id, active, completion_cooldown) else {
        return;
    };
    tokio::spawn(async move {
        let _lease = lease;
        // `get_automation` clones under a short store read. The owned context
        // means no daemon, store, or observation-table guard is held while
        // provider/tool execution runs.
        let Some(automation) = context.get_automation(&fire.automation_id).await else {
            return;
        };
        let Some(input) = input_if_still_current(&automation, &fire.reason) else {
            return;
        };
        let trigger = match &fire.reason {
            TriggerReason::Schedule { .. } => "schedule",
            TriggerReason::Event { .. } => "event",
            TriggerReason::Skill { .. } => "skill",
        };
        tracing::info!(automation = %automation.id, trigger, "automatic Automation firing");
        let result = crate::automation_executor::execute_automation_in_context(
            &context,
            &automation,
            &input,
        )
        .await;
        record_automation_outcome(&context, &automation, &result);
        if let Err(error) = result {
            tracing::warn!(
                automation = %automation.id,
                error = %error,
                "triggered Automation run failed"
            );
        }
    });
}

/// Start the canonical trigger runtime. Both `axocoatl dev` and
/// `axocoatl serve` call this exact entrypoint.
pub async fn start_automation_runtime(state: Arc<tokio::sync::RwLock<AxocoatlDaemon>>) {
    let (context, store, schedule_table, proactive_table, mut events) = {
        let daemon = state.read().await;
        (
            crate::automation_executor::AutomationExecutionContext::from_daemon(&daemon),
            daemon.automation_store.clone(),
            daemon.schedule_table.clone(),
            daemon.proactive_table.clone(),
            daemon.event_lattice.subscribe(),
        )
    };
    let active = Arc::new(Mutex::new(HashSet::new()));
    let cooldowns = Arc::new(Mutex::new(HashMap::new()));
    let initial = store.read().await.list();
    let mut planner = SchedulePlanner::default();
    let _ = planner.reconcile(&initial, &HashSet::new(), now_unix());
    reconcile_observations(&initial, &planner, &schedule_table, &proactive_table);

    tokio::spawn(async move {
        use tokio::sync::broadcast::error::RecvError;

        let mut ticker = tokio::time::interval(STORE_SCAN_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        ticker.tick().await;

        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    let automations = store.read().await.list();
                    let active_ids = active_snapshot(&active);
                    let fires = planner.reconcile(&automations, &active_ids, now_unix());
                    reconcile_observations(
                        &automations,
                        &planner,
                        &schedule_table,
                        &proactive_table,
                    );
                    if let Ok(mut cooldowns) = cooldowns.lock() {
                        cooldowns.retain(|id, _| automations.iter().any(|automation| automation.id == *id));
                    }
                    for fire in fires {
                        dispatch(context.clone(), fire, active.clone(), cooldowns.clone());
                    }
                }
                notification = events.recv() => {
                    let notification = match notification {
                        Ok(notification) => notification,
                        Err(RecvError::Lagged(skipped)) => {
                            tracing::warn!(skipped, "Automation trigger runtime lagged on lattice events");
                            continue;
                        }
                        Err(RecvError::Closed) => break,
                    };
                    let automations = store.read().await.list();
                    let active_ids = active_snapshot(&active);
                    let fires = cooldowns
                        .lock()
                        .map(|mut cooldowns| {
                            event_matches(
                                &automations,
                                &notification,
                                &active_ids,
                                &mut cooldowns,
                                now_unix(),
                            )
                        })
                        .unwrap_or_default();
                    for fire in fires {
                        dispatch(context.clone(), fire, active.clone(), cooldowns.clone());
                    }
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use axocoatl_config::{AutomationNode, NodeInput};
    use axocoatl_coordination::{EventId, EventType};

    fn automation(id: &str, trigger: AutomationTrigger) -> Automation {
        Automation {
            id: id.to_string(),
            name: id.to_string(),
            description: None,
            nodes: vec![AutomationNode {
                id: "agent".to_string(),
                kind: AutomationNodeKind::Agent {
                    agent_id: "coder".to_string(),
                    input: NodeInput::FromTrigger,
                },
                position: None,
            }],
            edges: Vec::new(),
            trigger,
            enabled: true,
            folder: None,
        }
    }

    fn notification(
        event: EventType,
        produced_by: &str,
        payload: serde_json::Value,
    ) -> EventNotification {
        EventNotification {
            event_id: EventId::new("event-1"),
            event_type: event,
            payload,
            produced_by: produced_by.to_string(),
            timestamp: 1,
        }
    }

    #[test]
    fn planner_reconciles_create_cadence_disable_and_delete_without_duplicates() {
        let mut planner = SchedulePlanner::default();
        let mut auto = automation(
            "timer",
            AutomationTrigger::Schedule {
                every: "10s".to_string(),
                input: Some("first".to_string()),
            },
        );
        assert!(planner
            .reconcile(&[auto.clone()], &HashSet::new(), 100)
            .is_empty());
        assert_eq!(planner.next_fire_unix("timer"), Some(110));

        let due = planner.reconcile(&[auto.clone()], &HashSet::new(), 110);
        assert_eq!(due.len(), 1);
        assert!(planner
            .reconcile(&[auto.clone()], &HashSet::new(), 110)
            .is_empty());

        auto.trigger = AutomationTrigger::Schedule {
            every: "2s".to_string(),
            input: Some("changed".to_string()),
        };
        assert!(planner
            .reconcile(&[auto.clone()], &HashSet::new(), 111)
            .is_empty());
        assert_eq!(planner.next_fire_unix("timer"), Some(113));

        auto.enabled = false;
        assert!(planner
            .reconcile(&[auto.clone()], &HashSet::new(), 113)
            .is_empty());
        auto.enabled = true;
        assert!(planner
            .reconcile(&[auto.clone()], &HashSet::new(), 114)
            .is_empty());
        assert_eq!(planner.next_fire_unix("timer"), Some(116));

        planner.reconcile(&[], &HashSet::new(), 115);
        assert_eq!(planner.next_fire_unix("timer"), None);
    }

    #[test]
    fn planner_skips_overlap_and_missed_backlog() {
        let auto = automation(
            "timer",
            AutomationTrigger::Schedule {
                every: "5s".to_string(),
                input: None,
            },
        );
        let mut planner = SchedulePlanner::default();
        planner.reconcile(std::slice::from_ref(&auto), &HashSet::new(), 10);
        let active = HashSet::from(["timer".to_string()]);
        assert!(planner
            .reconcile(std::slice::from_ref(&auto), &active, 15)
            .is_empty());
        assert_eq!(planner.next_fire_unix("timer"), Some(20));
        assert!(planner
            .reconcile(std::slice::from_ref(&auto), &HashSet::new(), 16)
            .is_empty());
    }

    #[test]
    fn event_and_skill_matching_are_exact_and_cooled_down() {
        let autos = vec![
            automation(
                "event-auto",
                AutomationTrigger::OnEvent {
                    event: "CodeReady".to_string(),
                    input: Some("fallback".to_string()),
                },
            ),
            automation(
                "skill-auto",
                AutomationTrigger::OnSkill {
                    skill_id: "review".to_string(),
                },
            ),
        ];
        let event = notification(
            EventType::Custom("CodeReady".to_string()),
            "skill:review",
            serde_json::json!({ "input": "payload" }),
        );
        let mut cooldowns = HashMap::new();
        let fires = event_matches(&autos, &event, &HashSet::new(), &mut cooldowns, 100);
        assert_eq!(fires.len(), 2);
        assert!(event_matches(&autos, &event, &HashSet::new(), &mut cooldowns, 101).is_empty());
        assert_eq!(
            event_matches(&autos, &event, &HashSet::new(), &mut cooldowns, 130).len(),
            2
        );

        let wrong_skill = notification(
            EventType::Custom("CodeReady".to_string()),
            "skill:reviewer",
            serde_json::Value::Null,
        );
        let mut clean = HashMap::new();
        let fires = event_matches(&autos, &wrong_skill, &HashSet::new(), &mut clean, 200);
        assert_eq!(fires.len(), 1);
        assert_eq!(fires[0].automation_id, "event-auto");
    }

    #[test]
    fn active_event_automation_does_not_loop() {
        let autos = vec![automation(
            "loop",
            AutomationTrigger::OnEvent {
                event: "Again".to_string(),
                input: None,
            },
        )];
        let external = notification(
            EventType::Custom("Again".to_string()),
            "user",
            serde_json::Value::Null,
        );
        let active = HashSet::from(["loop".to_string()]);
        assert!(event_matches(&autos, &external, &active, &mut HashMap::new(), 1).is_empty());
    }

    #[test]
    fn event_active_lease_extends_cooldown_on_completion() {
        let active = Arc::new(Mutex::new(HashSet::new()));
        let cooldowns = Arc::new(Mutex::new(HashMap::new()));
        {
            let _lease =
                ActiveLease::acquire("event-auto", active.clone(), Some(cooldowns.clone()))
                    .unwrap();
            assert!(active.lock().unwrap().contains("event-auto"));
        }
        assert!(!active.lock().unwrap().contains("event-auto"));
        assert!(cooldowns.lock().unwrap().contains_key("event-auto"));
    }

    #[test]
    fn preflight_uses_latest_input_and_rejects_changed_trigger() {
        let mut auto = automation(
            "timer",
            AutomationTrigger::Schedule {
                every: "5s".to_string(),
                input: Some("latest".to_string()),
            },
        );
        let reason = TriggerReason::Schedule {
            every: "5s".to_string(),
        };
        assert_eq!(
            input_if_still_current(&auto, &reason).as_deref(),
            Some("latest")
        );
        auto.trigger = AutomationTrigger::Schedule {
            every: "6s".to_string(),
            input: None,
        };
        assert_eq!(input_if_still_current(&auto, &reason), None);
    }

    #[test]
    fn payload_input_wins_then_fallback_then_json() {
        assert_eq!(
            event_input(
                &serde_json::json!({ "input": "event text" }),
                Some("fallback")
            ),
            "event text"
        );
        assert_eq!(
            event_input(&serde_json::json!({ "other": true }), Some("fallback")),
            "fallback"
        );
        assert_eq!(
            event_input(&serde_json::json!({ "other": true }), None),
            "{\"other\":true}"
        );
    }

    #[test]
    fn partial_execution_failure_is_not_recorded_as_ok() {
        let result = Ok(WorkflowOutput {
            workflow_id: "partial".into(),
            agent_outputs: Vec::new(),
            final_content: "downstream continued".into(),
            total_token_usage: axocoatl_core::TokenUsageStats::default(),
            completed_agents: vec!["after".into()],
            failed_agents: vec![("broken".into(), "provider unavailable".into())],
        });

        let (summary, error) = outcome_summary(&result);
        assert_eq!(summary, "FAIL · 1 failed step(s)");
        assert_eq!(error.as_deref(), Some("broken: provider unavailable"));
    }

    #[test]
    fn observation_reconciliation_preserves_runtime_results_but_drops_deleted_rows() {
        let schedule_table: ScheduleTable = Arc::new(Mutex::new(Vec::new()));
        let proactive_table: ProactiveTable = Arc::new(Mutex::new(Vec::new()));
        let schedule = automation(
            "sched:daily",
            AutomationTrigger::Schedule {
                every: "1h".to_string(),
                input: None,
            },
        );
        let event = automation(
            "pro:watch",
            AutomationTrigger::OnEvent {
                event: "CodeReady".to_string(),
                input: None,
            },
        );
        let mut planner = SchedulePlanner::default();
        planner.reconcile(&[schedule.clone(), event.clone()], &HashSet::new(), 10);
        reconcile_observations(
            &[schedule.clone(), event.clone()],
            &planner,
            &schedule_table,
            &proactive_table,
        );
        {
            let mut rows = schedule_table.lock().unwrap();
            rows[0].run_count = 4;
            rows[0].last_outcome = Some("OK".to_string());
        }
        reconcile_observations(
            std::slice::from_ref(&schedule),
            &planner,
            &schedule_table,
            &proactive_table,
        );
        let rows = schedule_table.lock().unwrap();
        assert_eq!(rows[0].config.id, "daily");
        assert_eq!(rows[0].run_count, 4);
        assert_eq!(rows[0].last_outcome.as_deref(), Some("OK"));
        assert!(proactive_table.lock().unwrap().is_empty());
    }
}
