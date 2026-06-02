//! Exposes a configured Skill to a session agent as a callable tool.
//!
//! Calling the tool fires the skill into the lattice — publishing its `emit`
//! events, the same mechanism as the `/api/skills/{id}/fire` route, but
//! reachable by an agent mid-session. This is Axocoatl's answer to "connectors":
//! a per-session allowlist of skills the agents may reach into the lattice with.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axocoatl_config::SkillConfigYaml;
use axocoatl_coordination::{EventId, EventLattice, EventType, LatticeEvent};
use axocoatl_tools::{BuiltinTool, ToolError};

/// A callable tool that fires one configured Skill into the lattice.
pub struct SkillTool {
    skill: SkillConfigYaml,
    event_lattice: Arc<EventLattice>,
    description: String,
}

impl SkillTool {
    pub fn new(skill: SkillConfigYaml, event_lattice: Arc<EventLattice>) -> Self {
        let description = format!(
            "Fire the '{}' skill — {}. Emits lattice events [{}] that can \
             activate other agents in the org.",
            skill.name,
            skill.description,
            skill.emits.join(", "),
        );
        Self {
            skill,
            event_lattice,
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
            self.event_lattice.publish(LatticeEvent {
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
