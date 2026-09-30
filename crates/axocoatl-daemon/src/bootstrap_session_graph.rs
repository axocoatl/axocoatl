//! Authenticated current-turn graph proposals, distinct from future team Apply.
use super::*;
use axocoatl_core::AgentId;
use axocoatl_session::control_command::CommandReceiptView;
use axocoatl_session::turn_contract::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HumanGraphEditRequest {
    pub schema_version: u32,
    pub command_id: CommandId,
    pub session_id: SessionId,
    pub turn_id: LogicalTurnId,
    pub execution_epoch_id: ExecutionEpochId,
    pub expected_turn_revision: u64,
    pub expected_graph_revision: u64,
    pub action: HumanGraphEditAction,
    pub agent: session_team::SessionTeamSlotEdit,
    pub task: String,
    pub dependencies: Vec<TurnNodeId>,
    pub replacement: Option<TurnNodeId>,
    pub rewire_dependents: Vec<TurnNodeId>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HumanGraphEditAction {
    Add,
    Replace,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HumanGraphEditApply {
    pub request: HumanGraphEditRequest,
    pub review_digest: String,
}
#[derive(Serialize)]
pub struct HumanGraphEditPreview {
    pub request: HumanGraphEditRequest,
    pub review_digest: String,
    pub graph: TurnGraphSnapshot,
    pub receipt: Option<CommandReceiptView>,
}
fn graph_error(error: impl std::fmt::Display) -> DaemonError {
    DaemonError::SessionConflict(error.to_string())
}
impl HumanGraphEditRequest {
    pub(crate) fn validate(&self) -> Result<(), DaemonError> {
        if self.schema_version != 1
            || self.expected_turn_revision == 0
            || self.expected_graph_revision == 0
            || self.task.trim().is_empty()
            || self.agent.template_id.is_none()
            || !self.agent.required
            || !self.agent.reset_history
            || self.agent.source_slot_id.is_some()
        {
            return Err(graph_error(
                "Choose an Agent and task for new required work with a fresh conversation",
            ));
        }
        if self.agent.limits.is_none() || self.agent.expires_at_ms.is_none() {
            return Err(graph_error(
                "Enter every explicit limit and the approval expiry",
            ));
        }
        if (self.action == HumanGraphEditAction::Add
            && (self.replacement.is_some() || !self.rewire_dependents.is_empty()))
            || (self.action == HumanGraphEditAction::Replace
                && (self.replacement.is_none() || !self.dependencies.is_empty()))
        {
            return Err(graph_error(
                "Graph edit has inconsistent add/replacement targets",
            ));
        }
        if serde_json::to_vec(self).map_err(graph_error)?.len()
            > axocoatl_session::control_command::MAX_CONTROL_REQUEST_BYTES
        {
            return Err(graph_error("Graph edit exceeds the request bound"));
        }
        Ok(())
    }
    pub(crate) fn identity(&self) -> Result<String, DaemonError> {
        Ok(format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(self).map_err(graph_error)?)
        ))
    }
}
impl AxocoatlDaemon {
    async fn prepare_human_graph_edit(
        &self,
        session_id: &str,
        turn_id: &str,
        request: HumanGraphEditRequest,
        review: Option<&str>,
    ) -> Result<HumanGraphEditPreview, DaemonError> {
        request.validate()?;
        self.require_runtime_admission()?;
        if request.session_id.as_str() != session_id || request.turn_id.as_str() != turn_id {
            return Err(graph_error("Graph edit belongs to another Session or turn"));
        }
        let token = self
            .session_dispatch_lifecycles
            .session_team_token(session_id)?;
        if let Some(previous) = self
            .session_dispatch_lifecycles
            .repeated_session_graph_edit(&token, &request, review)?
        {
            return Ok(previous);
        }
        // Only a new Apply may reacquire an actual Ready repository owner.
        // Inspection and exact receipt retries never perform that lifecycle action.
        if review.is_some() {
            self.ensure_registered_native_session(session_id).await?;
        }
        let token = self
            .session_dispatch_lifecycles
            .session_team_token(session_id)?;
        self.session_dispatch_lifecycles
            .with_session_team_controller(&token, |_| Ok(()))?;
        let id = request.identity()?;
        let template = request
            .agent
            .template_id
            .as_ref()
            .ok_or_else(|| graph_error("Select an Agent template"))?;
        let mut config = self
            .config
            .agents
            .iter()
            .find(|agent| &agent.id == template)
            .ok_or_else(|| graph_error("Selected Agent template is unavailable"))?
            .to_core();
        config.id = AgentId::new(format!("dynamic-conversation-{id}"));
        config.name = request.agent.name.clone();
        config.provider = request.agent.provider.clone();
        config.model = request.agent.model.clone();
        config.system_prompt = request.agent.instructions.clone();
        config.sampling.max_tokens = request.agent.max_output_tokens;
        let definition = self
            .prepare_native_session_team_definition(
                &token,
                config,
                AgentDefinitionId::new(format!("dynamic-definition-{id}")).map_err(graph_error)?,
                1,
                request
                    .agent
                    .limits
                    .clone()
                    .ok_or_else(|| graph_error("Explicit limits are required"))?,
            )
            .await?;
        self.session_dispatch_lifecycles
            .with_session_team_controller(&token, |controller| {
                controller
                    .prepare_human_graph_edit(request, definition.definition, review)
                    .map_err(graph_error)
            })
    }
    pub async fn preview_session_graph_edit(
        &self,
        id: &str,
        turn: &str,
        request: HumanGraphEditRequest,
    ) -> Result<HumanGraphEditPreview, DaemonError> {
        self.prepare_human_graph_edit(id, turn, request, None).await
    }
    pub async fn apply_session_graph_edit(
        &self,
        id: &str,
        turn: &str,
        request: HumanGraphEditApply,
    ) -> Result<HumanGraphEditPreview, DaemonError> {
        self.prepare_human_graph_edit(id, turn, request.request, Some(&request.review_digest))
            .await
    }
}
