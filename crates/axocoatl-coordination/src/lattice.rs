use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use axocoatl_core::AgentId;

use crate::pheromone::SignalState;

/// Unique event identifier.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EventId(pub String);

impl EventId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
    pub fn random() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }
}

/// A single event in the lattice.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LatticeEvent {
    pub id: EventId,
    pub event_type: EventType,
    pub payload: serde_json::Value,
    pub produced_by: String,
    pub timestamp: u64,
}

/// Types of events in the lattice.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum EventType {
    TaskAvailable { task_type: String },
    TaskCompleted { task_id: String },
    AgentActivated { agent_id: String },
    AgentFailed { agent_id: String, error: String },
    ToolResult { tool_name: String },
    UserInput,
    WorkflowCompleted,
    Custom(String),
}

/// Notification sent when an event is published.
#[derive(Debug, Clone)]
pub struct EventNotification {
    pub event_id: EventId,
    pub event_type: EventType,
    /// The published event's payload — carried so observers (e.g. the
    /// dashboard's SSE stream) can surface details like an agent's output.
    pub payload: serde_json::Value,
}

/// The typed event lattice — the shared coordination space.
/// Thread-safe: uses DashMap for concurrent signal tracking and broadcast for notifications.
pub struct EventLattice {
    events: DashMap<EventId, LatticeEvent>,
    signals: DashMap<AgentId, SignalState>,
    notify_tx: broadcast::Sender<EventNotification>,
}

impl EventLattice {
    pub fn new(channel_capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(channel_capacity);
        Self {
            events: DashMap::new(),
            signals: DashMap::new(),
            notify_tx: tx,
        }
    }

    /// Register an agent with its signal parameters.
    pub fn register_agent(&self, agent_id: AgentId, threshold: f32, decay_rate: f32) {
        self.signals
            .insert(agent_id, SignalState::new(threshold, decay_rate));
    }

    /// Publish an event to the lattice.
    /// Returns the list of agent IDs that should activate as a result.
    pub fn publish(&self, event: LatticeEvent) -> Vec<AgentId> {
        let event_id = event.id.clone();
        let event_type = event.event_type.clone();
        let payload = event.payload.clone();

        // Store the event
        self.events.insert(event_id.clone(), event);

        // Broadcast notification
        let _ = self.notify_tx.send(EventNotification {
            event_id,
            event_type: event_type.clone(),
            payload,
        });

        // Calculate signal strength based on event type
        let signal_strength = match &event_type {
            EventType::TaskAvailable { .. } => 1.0,
            EventType::TaskCompleted { .. } => 0.5,
            EventType::UserInput => 1.0,
            EventType::ToolResult { .. } => 0.3,
            EventType::AgentFailed { .. } => 0.8,
            EventType::WorkflowCompleted => 0.1,
            EventType::AgentActivated { .. } => 0.1,
            EventType::Custom(_) => 0.5,
        };

        // Update signals and check for activations
        let mut activated = Vec::new();
        for mut entry in self.signals.iter_mut() {
            entry.value_mut().add_signal(signal_strength);
            if entry.value_mut().should_activate() {
                activated.push(entry.key().clone());
            }
        }

        activated
    }

    /// Get an event by ID.
    pub fn get_event(&self, id: &EventId) -> Option<LatticeEvent> {
        self.events.get(id).map(|e| e.clone())
    }

    /// Subscribe to event notifications.
    pub fn subscribe(&self) -> broadcast::Receiver<EventNotification> {
        self.notify_tx.subscribe()
    }

    /// Broadcast an event to observers (SSE streams, dashboards) **without**
    /// affecting coordination — no signal accumulation, no activation, no
    /// storage. Use this for pure telemetry, e.g. an "agent starting" signal,
    /// so observability never perturbs the stigmergic cascade.
    pub fn notify_observers(&self, event: &LatticeEvent) {
        let _ = self.notify_tx.send(EventNotification {
            event_id: event.id.clone(),
            event_type: event.event_type.clone(),
            payload: event.payload.clone(),
        });
    }

    /// Number of events in the lattice.
    pub fn event_count(&self) -> usize {
        self.events.len()
    }

    /// Number of registered agents.
    pub fn agent_count(&self) -> usize {
        self.signals.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now_timestamp() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    }

    fn task_event(task_type: &str) -> LatticeEvent {
        LatticeEvent {
            id: EventId::random(),
            event_type: EventType::TaskAvailable {
                task_type: task_type.to_string(),
            },
            payload: serde_json::json!({}),
            produced_by: "test".to_string(),
            timestamp: now_timestamp(),
        }
    }

    #[test]
    fn publish_and_retrieve_event() {
        let lattice = EventLattice::new(100);
        let event = task_event("research");
        let event_id = event.id.clone();
        lattice.publish(event);

        assert_eq!(lattice.event_count(), 1);
        let retrieved = lattice.get_event(&event_id).unwrap();
        assert!(matches!(
            retrieved.event_type,
            EventType::TaskAvailable { .. }
        ));
    }

    #[test]
    fn agent_activation_on_threshold() {
        let lattice = EventLattice::new(100);
        // Agent with threshold 1.0 — a single TaskAvailable (strength 1.0) should activate it
        lattice.register_agent(AgentId::new("agent-1"), 1.0, 0.0);

        let activated = lattice.publish(task_event("research"));
        assert_eq!(activated.len(), 1);
        assert_eq!(activated[0], AgentId::new("agent-1"));
    }

    #[test]
    fn no_activation_below_threshold() {
        let lattice = EventLattice::new(100);
        // Agent with threshold 2.0 — needs 2 events to activate
        lattice.register_agent(AgentId::new("agent-1"), 2.0, 0.0);

        let activated = lattice.publish(task_event("a"));
        assert!(activated.is_empty());

        let activated = lattice.publish(task_event("b"));
        assert_eq!(activated.len(), 1); // Now threshold crossed
    }

    #[test]
    fn multiple_agents_independent() {
        let lattice = EventLattice::new(100);
        lattice.register_agent(AgentId::new("fast"), 0.5, 0.0); // Low threshold
        lattice.register_agent(AgentId::new("slow"), 5.0, 0.0); // High threshold

        let activated = lattice.publish(task_event("task"));
        // Only "fast" should activate (threshold 0.5, signal 1.0)
        assert_eq!(activated.len(), 1);
        assert_eq!(activated[0], AgentId::new("fast"));
    }

    #[tokio::test]
    async fn subscribe_receives_notifications() {
        let lattice = EventLattice::new(100);
        let mut rx = lattice.subscribe();

        lattice.publish(task_event("test"));

        let notif = rx.recv().await.unwrap();
        assert!(matches!(notif.event_type, EventType::TaskAvailable { .. }));
    }
}
