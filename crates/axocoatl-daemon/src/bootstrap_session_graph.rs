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

/// What work added to a running turn may change: the write scope of the
/// Agent it starts from, `base`, unless the request names a narrower one.
/// Leaving the field out never widens it, and a request that would is
/// refused.
fn added_work_writes(
    name: &str,
    requested: Option<&Option<Vec<String>>>,
    base: Option<Vec<String>>,
) -> Result<Option<Vec<String>>, DaemonError> {
    let Some(requested) = requested else {
        return Ok(base);
    };
    if axocoatl_session::path_scope::write_scope_within(requested.as_deref(), base.as_deref()) {
        return Ok(requested.clone());
    }
    let base = base.unwrap_or_default();
    Err(graph_error(if base.is_empty() {
        format!("{name} is a read-only helper; leave out writes to add it")
    } else {
        format!(
            "{name} may change only {}; choose paths from that list",
            base.join(", ")
        )
    }))
}
impl HumanGraphEditRequest {
    pub(crate) fn validate(&self) -> Result<(), DaemonError> {
        if self.schema_version != 1
            || self.expected_turn_revision == 0
            || self.expected_graph_revision == 0
            || self.task.trim().is_empty()
            || self.agent.template_id.is_some() == self.agent.source_slot_id.is_some()
            || !self.agent.required
            || !self.agent.reset_history
            || self.agent.definition.is_some()
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
        // New work starts from one of the Session team's Agents as it is now,
        // or from a configured template the team does not use.
        let mut config = match (&request.agent.source_slot_id, &request.agent.template_id) {
            (Some(slot), None) => self.session_team_slot_config(&token, slot)?,
            (None, Some(template)) => self
                .config
                .agents
                .iter()
                .find(|agent| &agent.id == template)
                .ok_or_else(|| graph_error("Selected Agent template is unavailable"))?
                .to_core(),
            _ => return Err(graph_error("Choose one Agent to start from")),
        };
        config.id = AgentId::new(format!("dynamic-conversation-{id}"));
        config.name = request.agent.name.clone();
        config.provider = request.agent.provider.clone();
        config.model = request.agent.model.clone();
        config.system_prompt = request.agent.instructions.clone();
        config.sampling.max_tokens = request.agent.max_output_tokens;
        config.writes = added_work_writes(
            &request.agent.name,
            request.agent.writes.as_ref(),
            config.writes.take(),
        )?;
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
    /// The current definition of one of the Session team's Agents.
    fn session_team_slot_config(
        &self,
        token: &super::session_dispatch::SessionTeamToken,
        slot_id: &str,
    ) -> Result<axocoatl_core::AgentConfig, DaemonError> {
        use axocoatl_session::execution_content::ActivationEvidenceContent;
        use axocoatl_session::execution_namespace::ExecutionComponent;
        use axocoatl_session::session_team::SessionTeamStore;
        self.session_dispatch_lifecycles
            .with_session_team_stores(token, |canonical, content, _| {
                let store = SessionTeamStore::open_owned(
                    canonical
                        .component_namespace(ExecutionComponent::SessionTeam)
                        .map_err(graph_error)?,
                    canonical,
                    content,
                    None,
                )
                .map_err(graph_error)?;
                let current = store
                    .current()
                    .map_err(graph_error)?
                    .ok_or_else(|| graph_error("This Session has no applied team"))?;
                let slot = current
                    .graph
                    .slots
                    .iter()
                    .find(|slot| slot.slot_id.as_str() == slot_id)
                    .ok_or_else(|| graph_error("That Agent is no longer in this Session's team"))?;
                match &content
                    .resolve_activation_evidence(&slot.definition.snapshot)
                    .map_err(graph_error)?
                {
                    ActivationEvidenceContent::Definition { configuration, .. } => {
                        serde_json::from_str(configuration).map_err(graph_error)
                    }
                    _ => Err(graph_error("The Agent's definition is unavailable")),
                }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(paths: &[&str]) -> Option<Vec<String>> {
        Some(paths.iter().map(|path| (*path).to_owned()).collect())
    }

    #[test]
    fn added_work_keeps_or_narrows_the_scope_of_the_agent_it_starts_from() {
        // Leaving the field out keeps the base, whatever it is.
        for base in [None, paths(&[]), paths(&["lib/"])] {
            assert_eq!(
                added_work_writes("Helper", None, base.clone()).unwrap(),
                base
            );
        }
        // An explicit value may only narrow it.
        assert_eq!(
            added_work_writes("Helper", Some(&paths(&[])), paths(&["lib/"])).unwrap(),
            paths(&[])
        );
        assert_eq!(
            added_work_writes("Helper", Some(&None), None).unwrap(),
            None
        );
        let widened = added_work_writes("Helper", Some(&None), paths(&[])).unwrap_err();
        assert!(
            widened.to_string().contains("read-only helper"),
            "{widened}"
        );
        let other =
            added_work_writes("Helper", Some(&paths(&["src/"])), paths(&["lib/"])).unwrap_err();
        assert!(
            other.to_string().contains("may change only lib/"),
            "{other}"
        );
    }

    #[test]
    fn added_work_starts_from_exactly_one_agent() {
        let request = |template: Option<&str>, slot: Option<&str>| HumanGraphEditRequest {
            schema_version: 1,
            command_id: CommandId::new("edit").unwrap(),
            session_id: SessionId::new("session").unwrap(),
            turn_id: LogicalTurnId::new("turn").unwrap(),
            execution_epoch_id: ExecutionEpochId::new("epoch").unwrap(),
            expected_turn_revision: 1,
            expected_graph_revision: 1,
            action: HumanGraphEditAction::Add,
            agent: session_team::SessionTeamSlotEdit {
                slot_id: "draft".into(),
                template_id: template.map(str::to_owned),
                source_slot_id: slot.map(str::to_owned),
                role: Default::default(),
                delegation: None,
                name: "Helper".into(),
                provider: "ollama".into(),
                model: "model".into(),
                instructions: None,
                max_output_tokens: None,
                writes: None,
                required: true,
                reset_history: true,
                limits: Some(axocoatl_session::control_authority::GrantLimits {
                    activations: 1,
                    invocations: 1,
                    tokens: 1,
                    cost_microunits: 0,
                }),
                expires_at_ms: Some(1),
                definition: None,
            },
            task: "Review".into(),
            dependencies: vec![],
            replacement: None,
            rewire_dependents: vec![],
        };
        assert!(request(Some("template"), None).validate().is_ok());
        assert!(request(None, Some("slot-helper")).validate().is_ok());
        assert!(request(None, None).validate().is_err());
        assert!(request(Some("template"), Some("slot-helper"))
            .validate()
            .is_err());
    }
}
