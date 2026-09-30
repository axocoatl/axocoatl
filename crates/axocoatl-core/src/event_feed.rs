//! The process-wide event feed.
//!
//! Skills publish typed events here; On-event and On-skill Automations,
//! outbound webhooks, the recent-events API and WebSocket `event` frames
//! subscribe. The feed only broadcasts: it keeps no history and starts no
//! work. Subscribers that need history keep their own.
//!
//! The daemon itself publishes one kind of event: [`EventType::Custom`], once
//! for each name in a Skill's `emits` list when the Skill is fired (through
//! `POST /api/skills/{id}/fire` or an Agent's `skill_<id>` tool). The other
//! [`EventType`] variants are kept for embedders that publish their own events
//! on a feed they own.

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

/// A single event published on the feed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeedEvent {
    pub id: EventId,
    pub event_type: EventType,
    pub payload: serde_json::Value,
    pub produced_by: String,
    pub timestamp: u64,
}

/// Types of events on the feed. The daemon publishes only `Custom` (a Skill's
/// declared event name); see the module documentation.
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

    /// Whether this event is pure observability telemetry. Webhooks leave
    /// telemetry out of their default "all events" set unless it is named.
    pub fn is_telemetry(&self) -> bool {
        matches!(self, EventType::AgentActivated { .. })
    }
}

/// Notification sent to subscribers when an event is published.
#[derive(Debug, Clone)]
pub struct EventNotification {
    pub event_id: EventId,
    pub event_type: EventType,
    /// The published event's payload, carried so observers can surface it.
    pub payload: serde_json::Value,
    /// The Skill or source that produced the event (`FeedEvent::produced_by`).
    pub produced_by: String,
    /// Unix-seconds timestamp when the event was produced.
    pub timestamp: u64,
}

/// The process-wide event feed: a broadcast channel with no history.
pub struct EventFeed {
    notify_tx: broadcast::Sender<EventNotification>,
}

impl EventFeed {
    pub fn new(channel_capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(channel_capacity);
        Self { notify_tx: tx }
    }

    /// Broadcast an event to every current subscriber.
    pub fn publish(&self, event: FeedEvent) {
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

impl From<EventNotification> for FeedEvent {
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

    fn skill_event(name: &str) -> FeedEvent {
        FeedEvent {
            id: EventId::random(),
            event_type: EventType::Custom(name.to_string()),
            payload: serde_json::json!({ "fired_by_skill": "review" }),
            produced_by: "skill:review".to_string(),
            timestamp: 1,
        }
    }

    #[test]
    fn a_published_event_reaches_every_subscriber_whole() {
        let feed = EventFeed::new(100);
        let mut first = feed.subscribe();
        let mut second = feed.subscribe();
        let event = skill_event("CodeReady");
        let event_id = event.id.clone();
        feed.publish(event);

        for rx in [&mut first, &mut second] {
            let received = FeedEvent::from(rx.try_recv().unwrap());
            assert_eq!(received.id, event_id);
            assert_eq!(received.produced_by, "skill:review");
            assert_eq!(received.event_type, EventType::Custom("CodeReady".into()));
            assert_eq!(received.payload["fired_by_skill"], "review");
        }
    }

    #[test]
    fn publishing_with_no_subscriber_keeps_nothing() {
        let feed = EventFeed::new(100);
        feed.publish(skill_event("Unheard"));
        let mut late = feed.subscribe();
        assert!(late.try_recv().is_err());
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
        // filters on its own name.
        assert_eq!(EventType::Custom("CodeReady".into()).name(), "CodeReady");
        // AgentActivated is the only pure-telemetry event.
        assert!(EventType::AgentActivated {
            agent_id: "a".into()
        }
        .is_telemetry());
        assert!(!EventType::Custom("CodeReady".into()).is_telemetry());
    }

    #[test]
    fn a_feed_event_keeps_its_1_0_json_shape() {
        let json = serde_json::to_value(skill_event("CodeReady")).unwrap();
        assert_eq!(
            json["event_type"],
            serde_json::json!({ "Custom": "CodeReady" })
        );
        assert_eq!(json["produced_by"], "skill:review");
        let failed = serde_json::to_value(EventType::AgentFailed {
            agent_id: "coder".into(),
            error: "timeout".into(),
        })
        .unwrap();
        assert_eq!(
            failed,
            serde_json::json!({ "AgentFailed": { "agent_id": "coder", "error": "timeout" } })
        );
    }
}
