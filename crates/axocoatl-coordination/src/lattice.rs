use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

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

impl EventType {
    /// Canonical, stable name for this event type — the string users filter on
    /// (e.g. in `webhooks[].events`) and the value placed in egress payloads.
    /// For `Custom`, the name is the custom event string itself, so a
    /// Skill-emitted event is filterable by its own name. Derived from a match,
    /// not `Debug`, so the contract never shifts with formatting.
    pub fn name(&self) -> &str {
        match self {
            EventType::TaskAvailable { .. } => "TaskAvailable",
            EventType::TaskCompleted { .. } => "TaskCompleted",
            EventType::AgentActivated { .. } => "AgentActivated",
            EventType::AgentFailed { .. } => "AgentFailed",
            EventType::ToolResult { .. } => "ToolResult",
            EventType::UserInput => "UserInput",
            EventType::WorkflowCompleted => "WorkflowCompleted",
            EventType::Custom(name) => name.as_str(),
        }
    }

    /// Whether this event is pure observability telemetry. Egress sinks
    /// exclude these from the default "all events" set so a webhook is not
    /// spammed on every agent activation.
    pub fn is_telemetry(&self) -> bool {
        matches!(self, EventType::AgentActivated { .. })
    }
}

/// Notification sent when an event is published.
#[derive(Debug, Clone)]
pub struct EventNotification {
    pub event_id: EventId,
    pub event_type: EventType,
    /// The published event's payload — carried so observers (e.g. the
    /// dashboard's SSE stream) can surface details like an agent's output.
    pub payload: serde_json::Value,
    /// The agent or source that produced the event (`LatticeEvent::produced_by`).
    pub produced_by: String,
    /// Unix-seconds timestamp when the event was produced.
    pub timestamp: u64,
}

/// The process-wide event feed: Skills, Automation triggers, webhooks and
/// the recent-events API publish to and subscribe from it. It keeps no
/// history and starts no work; subscribers that need history keep their own.
pub struct EventLattice {
    notify_tx: broadcast::Sender<EventNotification>,
}

impl EventLattice {
    pub fn new(channel_capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(channel_capacity);
        Self { notify_tx: tx }
    }

    /// Broadcast an event to every current subscriber.
    pub fn publish(&self, event: LatticeEvent) {
        let _ = self.notify_tx.send(EventNotification {
            event_id: event.id,
            event_type: event.event_type,
            payload: event.payload,
            produced_by: event.produced_by,
            timestamp: event.timestamp,
        });
    }

    /// Subscribe to event notifications.
    pub fn subscribe(&self) -> broadcast::Receiver<EventNotification> {
        self.notify_tx.subscribe()
    }
}

impl From<EventNotification> for LatticeEvent {
    fn from(notification: EventNotification) -> Self {
        Self {
            id: notification.event_id,
            event_type: notification.event_type,
            payload: notification.payload,
            produced_by: notification.produced_by,
            timestamp: notification.timestamp,
        }
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

    #[tokio::test]
    async fn a_published_event_reaches_subscribers_whole() {
        let lattice = EventLattice::new(100);
        let mut rx = lattice.subscribe();
        let event = task_event("research");
        let event_id = event.id.clone();
        lattice.publish(event);

        let received = LatticeEvent::from(rx.recv().await.unwrap());
        assert_eq!(received.id, event_id);
        assert_eq!(received.produced_by, "test");
        assert!(matches!(
            received.event_type,
            EventType::TaskAvailable { .. }
        ));
    }

    #[test]
    fn event_type_name_is_canonical() {
        assert_eq!(
            EventType::TaskCompleted {
                task_id: "t".into()
            }
            .name(),
            "TaskCompleted"
        );
        assert_eq!(EventType::WorkflowCompleted.name(), "WorkflowCompleted");
        // Custom events are named by their own string, so a Skill-emitted event
        // filters on its own name (the old Debug-parsing approach broke this).
        assert_eq!(EventType::Custom("CodeReady".into()).name(), "CodeReady");
        // AgentActivated is the only pure-telemetry event.
        assert!(EventType::AgentActivated {
            agent_id: "a".into()
        }
        .is_telemetry());
        assert!(!EventType::TaskCompleted {
            task_id: "t".into()
        }
        .is_telemetry());
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
