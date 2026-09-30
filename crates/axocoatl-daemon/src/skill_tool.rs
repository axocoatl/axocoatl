//! Exposes a configured Skill to a session agent as a callable tool.
//!
//! Calling the tool fires the skill — publishing its `emit` events on the
//! event feed, the same mechanism as the `/api/skills/{id}/fire` route, but
//! reachable by an agent mid-session. On-event Automations and webhooks are
//! what react to those events.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axocoatl_config::SkillConfigYaml;
use axocoatl_core::event_feed::{EventFeed, EventId, EventType, FeedEvent};
use axocoatl_tools::{BuiltinTool, ToolError};

/// A callable tool that fires one configured Skill onto the event feed.
pub struct SkillTool {
    skill: SkillConfigYaml,
    event_feed: Arc<EventFeed>,
    description: String,
}

impl SkillTool {
    pub fn new(skill: SkillConfigYaml, event_feed: Arc<EventFeed>) -> Self {
        let description = format!(
            "Fire the '{}' skill — {}. Emits the events [{}] for Automations \
             and webhooks that react to them.",
            skill.name,
            skill.description,
            skill.emits.join(", "),
        );
        Self {
            skill,
            event_feed,
            description,
        }
    }

    /// The tool name the LLM sees — `skill_<id>`.
    pub fn tool_name(&self) -> String {
        format!("skill_{}", self.skill.id)
    }
}

#[async_trait::async_trait]
impl BuiltinTool for SkillTool {
    fn concurrency_policy(&self) -> axocoatl_llm::ConcurrencyPolicy {
        // A skill is an orchestration surface and may invoke arbitrary tools or
        // mutate repository state; it cannot honestly be classified read-only.
        axocoatl_llm::ConcurrencyPolicy::Exclusive
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "properties": {} })
    }

    async fn execute(&self, _arguments: serde_json::Value) -> Result<serde_json::Value, ToolError> {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let mut emitted = Vec::new();
        for emit in &self.skill.emits {
            self.event_feed.publish(FeedEvent {
                id: EventId::random(),
                event_type: EventType::Custom(emit.clone()),
                payload: serde_json::json!({ "fired_by_skill": self.skill.id }),
                produced_by: format!("skill:{}", self.skill.id),
                timestamp: ts,
            });
            emitted.push(emit.clone());
        }
        Ok(serde_json::json!({
            "skill": self.skill.id,
            "fired": true,
            "emitted_events": emitted,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn firing_publishes_one_skill_event_per_declared_name_and_nothing_else() {
        let feed = Arc::new(EventFeed::new(16));
        let mut events = feed.subscribe();
        let skill = SkillConfigYaml {
            id: "review".into(),
            name: "Review".into(),
            description: "Ask for review".into(),
            emits: vec!["ReviewRequested".into(), "CodeReady".into()],
            reacts_to: vec!["Ignored".into()],
            agents: vec!["reviewer".into()],
            prompt: "Never run".into(),
        };
        let tool = SkillTool::new(skill, feed.clone());
        assert_eq!(tool.tool_name(), "skill_review");

        let result = tool.execute(serde_json::json!({})).await.unwrap();
        assert_eq!(
            result["emitted_events"],
            serde_json::json!(["ReviewRequested", "CodeReady"])
        );
        for name in ["ReviewRequested", "CodeReady"] {
            let event = events.try_recv().unwrap();
            assert_eq!(event.event_type, EventType::Custom(name.into()));
            assert_eq!(event.produced_by, "skill:review");
            assert_eq!(
                event.payload,
                serde_json::json!({ "fired_by_skill": "review" })
            );
        }
        assert!(events.try_recv().is_err());
    }
}
