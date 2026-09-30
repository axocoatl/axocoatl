//! A per-turn model choice preserves the applied Team's exact finite limits.
//! Only the model field changes; future Team configuration remains untouched.
use super::*;
use axocoatl_session::control_authority::ExecutionProfile;
use axocoatl_session::session_team::SessionTeamSlot;
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeTurnModelSelection {
    pub node_id: TurnNodeId,
    pub approved_definition: DefinitionSnapshotRef,
    pub approved_grant: EvidenceRef,
    pub definition: DefinitionSnapshotRef,
    pub selected_model: String,
}
fn definition_id(
    turn: &LogicalTurnId,
    node: &TurnNodeId,
    definition: &DefinitionSnapshotRef,
    model: &str,
) -> Result<AgentDefinitionId, DaemonError> {
    let bytes = serde_json::to_vec(&(turn, node, definition, model)).map_err(failure)?;
    AgentDefinitionId::new(format!("turn-model-{:x}", Sha256::digest(bytes))).map_err(failure)
}
impl AxocoatlDaemon {
    pub(in crate::bootstrap) async fn prepare_native_send_model_selection(
        &self,
        token: &session_dispatch::SessionTeamToken,
        source: &super::super::native_send::NativeSessionSend,
        turn: &LogicalTurnId,
    ) -> Result<Vec<NativeTurnModelSelection>, DaemonError> {
        let Some(model) = &source.model_override else {
            return Ok(Vec::new());
        };
        if model.trim().is_empty() {
            return Err(failure("Select a non-empty model"));
        }
        let selected = self.session_dispatch_lifecycles.with_session_team_stores(
            token,
            |canonical, content, _| {
                let team = SessionTeamStore::open_owned(
                    canonical
                        .component_namespace(ExecutionComponent::SessionTeam)
                        .map_err(failure)?,
                    canonical,
                    content,
                    None,
                )
                .map_err(failure)?;
                let revision = team.current().map_err(failure)?.ok_or_else(|| {
                    failure("Apply the Session Team and budget before selecting a model")
                })?;
                let mut selected = Vec::new();
                for slot in &revision.graph.slots {
                    if let Some(target) = &source.target_agent {
                        let template =
                            super::super::session_team::approved_template_for_slot(content, slot)?;
                        if target != slot.node_id.as_str()
                            && target != slot.definition.definition_id.as_str()
                            && template.as_ref() != Some(target)
                        {
                            continue;
                        }
                    }
                    let ActivationEvidenceContent::Definition {
                        profile,
                        configuration,
                        ..
                    } = content
                        .resolve_activation_evidence(&slot.definition.snapshot)
                        .map_err(failure)?
                    else {
                        return Err(failure("Applied Agent definition is unavailable"));
                    };
                    let grant = slot
                        .grant
                        .as_ref()
                        .ok_or_else(|| failure("The Agent has no approved execution limits"))?;
                    let ActivationEvidenceContent::Grant { policy } = content
                        .resolve_activation_evidence(grant)
                        .map_err(failure)?
                    else {
                        return Err(failure("Approved Agent grant is unavailable"));
                    };
                    let mut config: AgentConfig =
                        serde_json::from_str(configuration).map_err(failure)?;
                    config.model = model.clone();
                    selected.push((
                        slot.clone(),
                        config,
                        policy.limits.clone(),
                        profile.model == *model,
                    ));
                }
                if selected.is_empty() || (source.target_agent.is_some() && selected.len() != 1) {
                    return Err(failure(
                        "The selected Agent must identify exactly one applied Team slot",
                    ));
                }
                Ok(selected)
            },
        )?;
        let mut result = Vec::new();
        for (slot, config, limits, unchanged) in selected {
            if unchanged {
                continue;
            }
            let id = definition_id(turn, &slot.node_id, &slot.definition, model)?;
            let captured = self
                .prepare_native_session_team_definition(token, config, id, 1, limits)
                .await?;
            if captured.profile.model != *model {
                return Err(failure("The selected model resolved differently; select its exact available model name"));
            }
            result.push(NativeTurnModelSelection {
                node_id: slot.node_id,
                approved_definition: slot.definition,
                approved_grant: slot.grant.expect("checked approval"),
                definition: captured.definition,
                selected_model: model.clone(),
            });
        }
        Ok(result)
    }
}

/// Validate the immutable origin and permit exactly the authenticated model
/// substitution. The unchanged grant is still compared with the applied Team.
pub(super) fn selected_definition(
    request: &NativeFirstTurnRequest,
    slot: &SessionTeamSlot,
    content: &axocoatl_session::execution_content::ExecutionContentStore,
) -> Result<DefinitionSnapshotRef, DaemonError> {
    let Some(selection) = request
        .model_selections
        .iter()
        .find(|selection| selection.node_id == slot.node_id)
    else {
        return Ok(slot.definition.clone());
    };
    if request
        .model_selections
        .iter()
        .filter(|selection| selection.node_id == slot.node_id)
        .count()
        != 1
        || selection.approved_definition != slot.definition
        || slot.grant.as_ref() != Some(&selection.approved_grant)
        || selection.definition.definition_id
            != definition_id(
                &request.turn_id,
                &slot.node_id,
                &slot.definition,
                &selection.selected_model,
            )?
        || request
            .ingress
            .as_ref()
            .and_then(|source| source.get("model_override"))
            .and_then(|value| value.as_str())
            != Some(selection.selected_model.as_str())
    {
        return Err(failure(
            "Per-turn model approval differs from the exact Send or applied Team",
        ));
    }
    let ActivationEvidenceContent::Definition {
        configuration: original,
        ..
    } = content
        .resolve_activation_evidence(&slot.definition.snapshot)
        .map_err(failure)?
    else {
        return Err(failure("Approved model source is missing"));
    };
    let ActivationEvidenceContent::Definition {
        definition_id,
        revision,
        configuration,
        profile,
    } = content
        .resolve_activation_evidence(&selection.definition.snapshot)
        .map_err(failure)?
    else {
        return Err(failure("Selected model capture is missing"));
    };
    let mut expected: AgentConfig = serde_json::from_str(original).map_err(failure)?;
    expected.model = selection.selected_model.clone();
    let expected_profile = ExecutionProfile {
        definition: definition_id.as_str().into(),
        provider: expected.provider.clone(),
        model: expected.model.clone(),
        isolation: "in-process".into(),
        tools: expected.tools.clone(),
    };
    if *revision != 1
        || definition_id != &selection.definition.definition_id
        || serde_json::to_string(&expected).map_err(failure)? != *configuration
        || profile != &expected_profile
    {
        return Err(failure(
            "Model selection changed an unapproved Agent capability or output bound",
        ));
    }
    Ok(selection.definition.clone())
}

pub(super) fn grant_for_model(
    grant: &AuthorityGrant,
    original: &ExecutionProfile,
    selected: &ExecutionProfile,
    selection: Option<&NativeTurnModelSelection>,
) -> AuthorityGrant {
    let Some(selection) = selection else {
        return grant.clone();
    };
    let mut effective = grant.clone();
    effective.id = format!(
        "model:{}",
        selection
            .definition
            .definition_id
            .as_str()
            .trim_start_matches("turn-model-")
    );
    for profile in &mut effective.profiles {
        if profile == original {
            *profile = selected.clone();
        }
    }
    effective
}
