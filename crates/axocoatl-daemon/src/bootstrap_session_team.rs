//! Human-reviewed future Session configuration. Preview retains immutable inputs;
//! only Apply advances the configuration journal. It never changes a live turn.
use super::*;
use crate::session_dispatch::NativeCoordinatorWorker;
use axocoatl_core::{AgentConfig, AgentId, AgentRole};
use axocoatl_session::control_authority::{AuthorityGrant, ExecutionProfile, GrantLimits};
use axocoatl_session::execution_content::{ActivationEvidenceContent, ExecutionContentStore};
use axocoatl_session::execution_namespace::ExecutionComponent;
use axocoatl_session::session_team::*;
use axocoatl_session::turn_contract::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

fn is_autonomous(role: &AgentRole) -> bool {
    *role == AgentRole::Autonomous
}
/// A present field, `null` included, is `Some`; only an absent one is `None`.
fn explicit_value<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}
fn team_error(value: impl std::fmt::Display) -> DaemonError {
    DaemonError::SessionConflict(value.to_string())
}
fn digest(value: &impl Serialize) -> Result<String, DaemonError> {
    Ok(format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(value).map_err(team_error)?)
    ))
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SessionTeamSlotEdit {
    pub slot_id: String,
    /// None retains the exact current definition; Some explicitly selects a template.
    pub template_id: Option<String>,
    #[serde(default)]
    pub source_slot_id: Option<String>,
    #[serde(default, skip_serializing_if = "is_autonomous")]
    pub role: AgentRole,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delegation: Option<CoordinatorApprovalEdit>,
    pub name: String,
    pub provider: String,
    pub model: String,
    pub instructions: Option<String>,
    pub max_output_tokens: Option<usize>,
    /// Repository paths this Agent may change. `null` lets it change any
    /// file, an empty list makes it read-only and a list names the paths.
    /// Leaving the field out keeps the scope of the template or definition
    /// the slot starts from, so omitting it never widens what the Agent may
    /// change. The app always sends the value.
    #[serde(
        default,
        deserialize_with = "explicit_value",
        skip_serializing_if = "Option::is_none"
    )]
    pub writes: Option<Option<Vec<String>>>,
    pub required: bool,
    pub reset_history: bool,
    pub limits: Option<GrantLimits>,
    pub expires_at_ms: Option<u64>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CoordinatorWorkerEdit {
    pub template_id: String,
    pub limits: GrantLimits,
    pub max_output_tokens: Option<usize>,
    pub adhoc_allowed: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CoordinatorApprovalEdit {
    pub workers: Vec<CoordinatorWorkerEdit>,
    pub operations: Vec<DelegatedOperation>,
    pub max_nodes: u32,
    pub max_edges: u32,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub(crate) struct ApprovedCoordinatorResource {
    pub session_id: String,
    pub workspace_id: String,
    pub working_dir: std::path::PathBuf,
    pub environment_generation: u64,
    pub backend: String,
    pub network: String,
    pub require_resource_limits: bool,
    pub image: Option<String>,
    pub setup_command: Option<String>,
    pub setup_approved: bool,
    pub setup_reviewed: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub(crate) struct ApprovedCoordinatorPolicy {
    pub workers: Vec<NativeCoordinatorWorker>,
    pub operations: Vec<DelegatedOperation>,
    pub max_nodes: u32,
    pub max_edges: u32,
    pub resource: ApprovedCoordinatorResource,
    /// HTN methods, removed in 1.1.0. New approvals write `null`; a value in
    /// an approval retained by an earlier build still parses and is ignored.
    #[serde(rename = "htn_methods_yaml")]
    pub legacy_htn_methods_yaml: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SessionTeamEdit {
    pub command_id: String,
    pub expected_configuration_revision: u64,
    pub slots: Vec<SessionTeamSlotEdit>,
    /// Edges use visible slot identities; the daemon resolves exact node identities.
    pub dependencies: Vec<SessionTeamConnection>,
    pub layout: Vec<SessionTeamPosition>,
    /// Exact commands (argv) the host runs after every turn's required
    /// Agents finish. A failure leaves the turn needing attention; passing
    /// checks on an unchanged tree let it complete. Empty means no checks,
    /// and keeps the historical serialized shape.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub required_checks: Vec<Vec<String>>,
    /// A read-only Worker template the host runs after the required Agents
    /// finish and the required checks pass. The turn completes only when it
    /// approves the exact result. Absent means no review, and keeps the
    /// historical serialized shape.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required_review: Option<ReviewSetting>,
}
fn default_review_rounds() -> u32 {
    axocoatl_session::turn_review::DEFAULT_REVIEW_ROUNDS
}
/// Which reviewer the host runs on every turn's result, how many rounds of
/// changes it sends back to the lead before the person decides, and the
/// reviewer's budget for all rounds of one turn.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ReviewSetting {
    pub template_id: String,
    #[serde(default = "default_review_rounds")]
    pub max_rounds: u32,
    pub limits: GrantLimits,
    /// The reviewer's output bound per request; absent keeps the template's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<usize>,
}
/// The reviewer an Apply approved: its setting and the exact definition the
/// host runs.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub(crate) struct ApprovedReview {
    pub template_id: String,
    pub definition: DefinitionSnapshotRef,
    pub max_rounds: u32,
    pub limits: GrantLimits,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SessionTeamConnection {
    pub parent: String,
    pub child: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionTeamApply {
    pub edit: SessionTeamEdit,
    pub review_digest: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionTeamCancel {
    pub command_id: String,
}
#[derive(Serialize)]
pub struct SessionTeamView {
    pub history_version: &'static str,
    pub configuration_revision: u64,
    pub slots: Vec<SessionTeamSlotEdit>,
    pub dependencies: Vec<SessionTeamConnection>,
    pub layout: Vec<SessionTeamPosition>,
    pub templates: Vec<SessionTeamSlotEdit>,
    pub approved: bool,
    /// The current applied team's required checks, as argv.
    pub required_checks: Vec<Vec<String>>,
    /// The current applied team's required review, when it has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub required_review: Option<ReviewSetting>,
    /// Templates that may review: read-only Workers.
    pub reviewers: Vec<String>,
    /// The Session's detected check command, offered as a check the person
    /// may add. It is only a suggestion: nothing runs it unless an Apply
    /// includes it.
    pub suggested_check: Option<Vec<String>>,
}
#[derive(Serialize)]
pub struct SessionTeamChange {
    pub slot_id: String,
    pub kind: String,
    pub history: String,
}
#[derive(Serialize)]
pub struct SessionTeamPreview {
    pub edit: SessionTeamEdit,
    pub review_digest: String,
    pub changes: Vec<SessionTeamChange>,
    pub configuration_revision: u64,
    pub applies_to: &'static str,
    pub profiles: Vec<ExecutionProfile>,
    coordinators: Vec<(String, ApprovedCoordinatorPolicy)>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SessionTeamApproval {
    kind: String,
    edit: SessionTeamEdit,
    templates: Vec<(String, Option<String>)>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    coordinators: Vec<(String, ApprovedCoordinatorPolicy)>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    review: Option<ApprovedReview>,
}
fn approval_for_slot(
    content: &ExecutionContentStore,
    slot: &SessionTeamSlot,
) -> Result<Option<SessionTeamApproval>, DaemonError> {
    let Some(reference) = &slot.grant else {
        return Ok(None);
    };
    let ActivationEvidenceContent::Grant { policy } = content
        .resolve_activation_evidence(reference)
        .map_err(team_error)?
    else {
        return Err(team_error("Session grant is unavailable"));
    };
    let ActivationEvidenceContent::Guidance { text } = content
        .resolve_activation_evidence(&policy.issuer_evidence)
        .map_err(team_error)?
    else {
        return Err(team_error("Session grant issuer is unavailable"));
    };
    match serde_json::from_str::<SessionTeamApproval>(text) {
        Ok(approval) if approval.kind == "authenticated_session_team_apply" => Ok(Some(approval)),
        _ => Ok(None),
    }
}
/// The required checks of the Apply that approved this slot's grant.
pub(crate) fn approved_required_checks(
    content: &ExecutionContentStore,
    slot: &SessionTeamSlot,
) -> Result<Vec<Vec<String>>, DaemonError> {
    Ok(approval_for_slot(content, slot)?
        .map(|approval| approval.edit.required_checks)
        .unwrap_or_default())
}
/// The required review of the Apply that approved this slot's grant. Its
/// retained definition must be the setting the Apply names.
pub(crate) fn approved_review(
    content: &ExecutionContentStore,
    slot: &SessionTeamSlot,
) -> Result<Option<ApprovedReview>, DaemonError> {
    let Some(approval) = approval_for_slot(content, slot)? else {
        return Ok(None);
    };
    match (approval.review, approval.edit.required_review) {
        (None, None) => Ok(None),
        (Some(review), Some(setting))
            if review.template_id == setting.template_id
                && review.max_rounds == setting.max_rounds
                && review.limits == setting.limits =>
        {
            Ok(Some(review))
        }
        _ => Err(team_error(
            "The approved required review differs from its setting; apply the team again",
        )),
    }
}
/// Whether `config` may review: a Worker that cannot change files or run
/// commands that could, the same rule `delegate` applies to helpers.
fn review_refusal(template_id: &str, config: &AgentConfig) -> Option<String> {
    if config.role != AgentRole::Worker {
        return Some(format!(
            "The reviewer {template_id} is not a Worker template. Choose a Worker template \
             that cannot change files"
        ));
    }
    let changing = crate::session_dispatch::changing_tools(&config.tools, config.writes.as_deref());
    (!changing.is_empty()).then(|| {
        format!(
            "The reviewer {template_id} can change files or run commands ({}). Only a \
             read-only Worker can review: set writes: [] on it, or choose another template",
            changing.join(", ")
        )
    })
}
pub(crate) fn approved_template_for_slot(
    content: &ExecutionContentStore,
    slot: &SessionTeamSlot,
) -> Result<Option<String>, DaemonError> {
    Ok(approval_for_slot(content, slot)?.and_then(|approval| {
        approval
            .templates
            .into_iter()
            .find(|(id, _)| id == slot.slot_id.as_str())
            .and_then(|(_, template)| template)
    }))
}

pub(crate) fn approved_coordinator_policy(
    content: &ExecutionContentStore,
    grant: &AuthorityGrant,
) -> Result<Option<ApprovedCoordinatorPolicy>, DaemonError> {
    let ActivationEvidenceContent::Guidance { text } = content
        .resolve_activation_evidence(&grant.issuer_evidence)
        .map_err(team_error)?
    else {
        return Ok(None);
    };
    let Ok(approval) = serde_json::from_str::<SessionTeamApproval>(text) else {
        return Ok(None);
    };
    if approval.kind != "authenticated_session_team_apply" {
        return Ok(None);
    }
    for (slot, policy) in approval.coordinators {
        let expected = format!(
            "team-node-{}",
            &digest(&(&policy.resource.session_id, &slot))?[..24]
        );
        if grant.holder.as_str() == expected {
            return Ok(Some(policy));
        }
    }
    Ok(None)
}

fn review_profiles(
    content: &ExecutionContentStore,
    graph: &SessionTeamGraph,
) -> Result<Vec<ExecutionProfile>, DaemonError> {
    let mut profiles = Vec::new();
    for slot in &graph.slots {
        let reference = slot
            .grant
            .as_ref()
            .ok_or_else(|| team_error("Reviewed grant is missing"))?;
        let ActivationEvidenceContent::Grant { policy } = content
            .resolve_activation_evidence(reference)
            .map_err(team_error)?
        else {
            return Err(team_error("Reviewed grant is invalid"));
        };
        for profile in &policy.profiles {
            if !profiles.contains(profile) {
                profiles.push(profile.clone());
            }
        }
    }
    Ok(profiles)
}
/// The paths the slot may change after checking the pattern grammar: its
/// explicit `writes`, or `base`, the scope of the template or definition it
/// starts from, when the edit leaves the field out.
fn checked_writes(
    slot: &SessionTeamSlotEdit,
    base: Option<&[String]>,
) -> Result<Option<Vec<String>>, DaemonError> {
    let Some(writes) = &slot.writes else {
        return Ok(base.map(<[String]>::to_vec));
    };
    if let Some(writes) = writes {
        axocoatl_session::path_scope::validate_write_scope(writes).map_err(|reason| {
            team_error(format!(
                "{} has an invalid list of paths it may change: {reason}. Use repository paths \
                 such as lib/ or docs/*.md, or choose Nothing for a read-only helper",
                slot.name
            ))
        })?;
    }
    Ok(writes.clone())
}
/// The team's required checks must be exact commands that fit beside the
/// team's own conditions.
fn check_required_checks(edit: &SessionTeamEdit, conditions: usize) -> Result<(), DaemonError> {
    let most = MAX_COMPLETION_CONDITIONS.saturating_sub(conditions + 3);
    if edit.required_checks.len() > most {
        return Err(team_error(format!(
            "A team can have at most {most} required checks; remove some"
        )));
    }
    axocoatl_session::turn_checks::check_definitions(&edit.required_checks).map_err(|_| {
        team_error(
            "Each required check must be a command of at most 64 arguments, each at most \
             4096 bytes and without NUL characters",
        )
    })?;
    Ok(())
}
/// A required review names 1 to 3 rounds and a reviewer budget that pays for
/// every round: one activation each, and invocations for its answer and, when
/// it has bash, the repository captures around it.
fn check_required_review(edit: &SessionTeamEdit) -> Result<(), DaemonError> {
    use axocoatl_session::turn_review::MAX_REVIEW_ROUNDS;
    let Some(review) = &edit.required_review else {
        return Ok(());
    };
    if !(1..=MAX_REVIEW_ROUNDS).contains(&review.max_rounds) {
        return Err(team_error(format!(
            "A required review runs 1 to {MAX_REVIEW_ROUNDS} rounds; choose how many"
        )));
    }
    if review.limits.activations < review.max_rounds
        || review.limits.invocations < review.max_rounds
        || review.limits.tokens == 0
    {
        return Err(team_error(format!(
            "The reviewer runs once per round, so its budget needs at least {} activations, \
             {} invocations and some tokens",
            review.max_rounds, review.max_rounds
        )));
    }
    Ok(())
}
/// The invocations a reviewer spends per round: its answer, and the
/// repository captures around it when it has bash.
fn review_round_invocations(config: &AgentConfig) -> u32 {
    if config.tools.iter().any(|tool| tool == "bash") {
        3
    } else {
        1
    }
}
/// How a write scope reads in a message to the person.
fn describe_writes(writes: Option<&[String]>) -> String {
    match writes {
        None => "any file".into(),
        Some([]) => "nothing".into(),
        Some(paths) => paths.join(", "),
    }
}
fn slot_edit(
    slot: &SessionTeamSlot,
    content: &ExecutionContentStore,
) -> Result<SessionTeamSlotEdit, DaemonError> {
    let ActivationEvidenceContent::Definition { configuration, .. } = content
        .resolve_activation_evidence(&slot.definition.snapshot)
        .map_err(team_error)?
    else {
        return Err(team_error("Session definition is unavailable"));
    };
    let config: AgentConfig = serde_json::from_str(configuration).map_err(team_error)?;
    let ActivationEvidenceContent::Budget { limits } = content
        .resolve_activation_evidence(&slot.budget)
        .map_err(team_error)?
    else {
        return Err(team_error("Session budget is unavailable"));
    };
    let expires_at_ms = match &slot.grant {
        Some(reference) => match content
            .resolve_activation_evidence(reference)
            .map_err(team_error)?
        {
            ActivationEvidenceContent::Grant { policy } => Some(policy.expires_at_ms),
            _ => return Err(team_error("Session grant is unavailable")),
        },
        None => None,
    };
    Ok(SessionTeamSlotEdit {
        slot_id: slot.slot_id.as_str().into(),
        template_id: None,
        source_slot_id: None,
        role: config.role,
        delegation: approval_for_slot(content, slot)?.and_then(|approval| {
            approval
                .edit
                .slots
                .into_iter()
                .find(|edit| edit.slot_id == slot.slot_id.as_str())
                .and_then(|edit| edit.delegation)
        }),
        name: config.name,
        provider: config.provider,
        model: config.model,
        instructions: config.system_prompt,
        max_output_tokens: config.sampling.max_tokens,
        writes: Some(config.writes),
        required: slot.required,
        reset_history: false,
        limits: Some(limits.clone()),
        expires_at_ms,
    })
}
fn connections(revision: &SessionTeamRevision) -> Vec<SessionTeamConnection> {
    revision
        .graph
        .dependencies
        .iter()
        .filter_map(|edge| {
            let parent = revision
                .graph
                .slots
                .iter()
                .find(|slot| slot.node_id == edge.parent)?;
            let child = revision
                .graph
                .slots
                .iter()
                .find(|slot| slot.node_id == edge.child)?;
            Some(SessionTeamConnection {
                parent: parent.slot_id.as_str().into(),
                child: child.slot_id.as_str().into(),
            })
        })
        .collect()
}
impl AxocoatlDaemon {
    pub async fn session_team(&self, session_id: &str) -> Result<SessionTeamView, DaemonError> {
        let session = self
            .get_session(session_id)
            .await
            .ok_or_else(|| team_error("Session does not exist"))?;
        if !self.uses_native_session_history() {
            return Ok(SessionTeamView {
                history_version: "legacy_v1",
                configuration_revision: 0,
                slots: vec![],
                dependencies: vec![],
                layout: vec![],
                templates: vec![],
                approved: false,
                required_checks: vec![],
                required_review: None,
                reviewers: vec![],
                suggested_check: None,
            });
        }
        let suggested_check = session
            .check_command
            .clone()
            .map(|command| vec!["sh".into(), "-c".into(), command]);
        let token = self
            .session_dispatch_lifecycles
            .session_team_token(session_id)?;
        let templates: Vec<_> = self
            .config
            .agents
            .iter()
            .map(|agent| {
                let config = agent.to_core();
                SessionTeamSlotEdit {
                    slot_id: format!("slot-{}", agent.id),
                    template_id: Some(agent.id.clone()),
                    source_slot_id: None,
                    role: config.role,
                    delegation: None,
                    name: config.name,
                    provider: config.provider,
                    model: config.model,
                    instructions: config.system_prompt,
                    max_output_tokens: config.sampling.max_tokens,
                    writes: Some(config.writes),
                    required: true,
                    reset_history: true,
                    limits: None,
                    expires_at_ms: None,
                }
            })
            .collect();
        let reviewers: Vec<String> = self
            .config
            .agents
            .iter()
            .filter(|agent| review_refusal(&agent.id, &agent.to_core()).is_none())
            .map(|agent| agent.id.clone())
            .collect();
        self.session_dispatch_lifecycles.with_session_team_stores(
            &token,
            |canonical, content, _| {
                let store = SessionTeamStore::open_owned(
                    canonical
                        .component_namespace(ExecutionComponent::SessionTeam)
                        .map_err(team_error)?,
                    canonical,
                    content,
                    None,
                )
                .map_err(team_error)?;
                if let Some(current) = store.current().map_err(team_error)? {
                    let required_checks = match current.graph.slots.first() {
                        Some(slot) => approved_required_checks(content, slot)?,
                        None => vec![],
                    };
                    let required_review = match current.graph.slots.first() {
                        Some(slot) => approval_for_slot(content, slot)?
                            .and_then(|approval| approval.edit.required_review),
                        None => None,
                    };
                    return Ok(SessionTeamView {
                        history_version: "execution_v2",
                        configuration_revision: current.configuration_revision,
                        slots: current
                            .graph
                            .slots
                            .iter()
                            .map(|slot| slot_edit(slot, content))
                            .collect::<Result<_, _>>()?,
                        dependencies: connections(current),
                        layout: current.layout.clone(),
                        templates,
                        approved: current.graph.slots.iter().all(|slot| slot.grant.is_some()),
                        required_checks,
                        required_review,
                        reviewers,
                        suggested_check,
                    });
                }
                let selected: Vec<String> = match &session.mode {
                    axocoatl_session::SessionMode::SingleAgent { agent_id } => {
                        vec![agent_id.clone()]
                    }
                    axocoatl_session::SessionMode::Custom { agents } => agents.clone(),
                    axocoatl_session::SessionMode::Lattice { workflow_id } => self
                        .config
                        .workflows
                        .iter()
                        .find(|workflow| workflow_id.as_ref().is_none_or(|id| id == &workflow.id))
                        .map(|workflow| workflow.agents.clone())
                        .ok_or_else(|| team_error("Session team configuration is missing"))?,
                };
                let slots = selected
                    .iter()
                    .map(|id| {
                        templates
                            .iter()
                            .find(|slot| slot.template_id.as_ref() == Some(id))
                            .cloned()
                            .ok_or_else(|| {
                                team_error(format!("Session Agent {id} is no longer configured"))
                            })
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                let dependencies = selected
                    .iter()
                    .flat_map(|id| {
                        self.config
                            .agents
                            .iter()
                            .filter(move |agent| &agent.id == id)
                    })
                    .flat_map(|agent| {
                        agent
                            .depends_on
                            .iter()
                            .map(move |parent| SessionTeamConnection {
                                parent: format!("slot-{parent}"),
                                child: format!("slot-{}", agent.id),
                            })
                    })
                    .collect();
                Ok(SessionTeamView {
                    history_version: "execution_v2",
                    configuration_revision: 0,
                    slots,
                    dependencies,
                    layout: vec![],
                    templates,
                    approved: false,
                    required_checks: vec![],
                    required_review: None,
                    reviewers,
                    suggested_check,
                })
            },
        )
    }

    async fn prepare_coordinator_approvals(
        &self,
        session_id: &str,
        edit: &SessionTeamEdit,
        previous: Option<&SessionTeamRevision>,
        token: &super::session_dispatch::SessionTeamToken,
    ) -> Result<Vec<(String, ApprovedCoordinatorPolicy)>, DaemonError> {
        let session = self
            .get_session(session_id)
            .await
            .ok_or_else(|| team_error("Session is missing"))?;
        let mut approved = Vec::new();
        for slot in &edit.slots {
            let prior = previous.and_then(|revision| {
                revision.graph.slots.iter().find(|entry| {
                    entry.slot_id.as_str() == slot.slot_id
                        || slot.source_slot_id.as_deref() == Some(entry.slot_id.as_str())
                })
            });
            let previous_approval = prior
                .map(|entry| {
                    self.session_dispatch_lifecycles
                        .with_session_team_stores(token, |_, content, _| {
                            approval_for_slot(content, entry)
                        })
                })
                .transpose()?
                .flatten();
            let parent_config = if let Some(id) = &slot.template_id {
                self.config
                    .agents
                    .iter()
                    .find(|agent| &agent.id == id)
                    .ok_or_else(|| team_error("Coordinator template is missing"))?
                    .to_core()
            } else {
                let prior =
                    prior.ok_or_else(|| team_error("New Agent needs an explicit template"))?;
                self.session_dispatch_lifecycles.with_session_team_stores(
                    token,
                    |_, content, _| {
                        let ActivationEvidenceContent::Definition { configuration, .. } = content
                            .resolve_activation_evidence(&prior.definition.snapshot)
                            .map_err(team_error)?
                        else {
                            return Err(team_error("Retained Agent definition is missing"));
                        };
                        serde_json::from_str::<AgentConfig>(configuration).map_err(team_error)
                    },
                )?
            };
            let Some(proposal) = &slot.delegation else {
                if parent_config.role == AgentRole::Coordinator {
                    return Err(team_error(
                        "Approve Worker templates, operations and graph limits for the Coordinator",
                    ));
                }
                continue;
            };
            if parent_config.role == AgentRole::Worker {
                return Err(team_error(
                    "A Worker cannot delegate; remove its helpers or choose an Autonomous or Coordinator template",
                ));
            }
            if !session.environment.setup_reviewed {
                return Err(team_error(
                    "Review this Session's environment before approving helpers",
                ));
            }
            if proposal.workers.is_empty()
                || proposal.workers.len() > MAX_CONTRACT_NODES
                || proposal.max_nodes == 0
                || proposal.max_nodes as usize > MAX_CONTRACT_NODES
                || proposal.max_edges as usize > MAX_GRAPH_EDGES
                || !proposal.operations.contains(&DelegatedOperation::AddAgent)
            {
                return Err(team_error(
                    "Select Worker templates, Add work permission and explicit graph bounds",
                ));
            }
            let mut seen_operations = HashSet::new();
            if proposal
                .operations
                .iter()
                .any(|operation| !seen_operations.insert(*operation))
            {
                return Err(team_error("Delegated operations must be unique"));
            }
            let aggregate = slot.limits.as_ref().ok_or_else(|| {
                team_error("An Agent that delegates needs explicit aggregate limits")
            })?;
            let old_policy = previous_approval.as_ref().and_then(|approval| {
                approval
                    .coordinators
                    .iter()
                    .find(|(id, _)| prior.is_some_and(|entry| entry.slot_id.as_str() == id))
                    .map(|(_, policy)| policy)
            });
            let old_edit = previous_approval
                .as_ref()
                .and_then(|approval| {
                    approval.edit.slots.iter().find(|entry| {
                        prior.is_some_and(|old| old.slot_id.as_str() == entry.slot_id)
                    })
                })
                .and_then(|entry| entry.delegation.as_ref());
            let mut workers = Vec::new();
            let mut ids = HashSet::new();
            for worker in &proposal.workers {
                if !ids.insert(worker.template_id.clone())
                    || worker.limits.activations == 0
                    || worker.limits.invocations == 0
                    || worker.limits.tokens == 0
                    || worker.limits.activations > aggregate.activations
                    || worker.limits.invocations > aggregate.invocations
                    || worker.limits.tokens > aggregate.tokens
                    || worker.limits.cost_microunits > aggregate.cost_microunits
                {
                    return Err(team_error("Each helper needs a unique Worker template and explicit limits within the delegating Agent's aggregate"));
                }
                let preserved = if slot.template_id.is_none()
                    && old_edit.is_some_and(|old| {
                        old.workers.iter().any(|entry| {
                            entry.template_id == worker.template_id
                                && entry.max_output_tokens == worker.max_output_tokens
                        })
                    }) {
                    old_policy
                        .and_then(|old| {
                            old.workers
                                .iter()
                                .find(|entry| entry.template_id == worker.template_id)
                        })
                        .map(|entry| entry.definition.clone())
                } else {
                    None
                };
                let definition = if let Some(definition) = preserved {
                    definition
                } else {
                    let mut config = self
                        .config
                        .agents
                        .iter()
                        .find(|agent| agent.id == worker.template_id)
                        .ok_or_else(|| team_error("Selected Worker template is missing"))?
                        .to_core();
                    if config.role != AgentRole::Worker {
                        return Err(team_error("Helpers must use an actual Worker template"));
                    }
                    config.sampling.max_tokens = worker.max_output_tokens;
                    let identity = digest(&(session_id, &edit.command_id, &slot.slot_id, worker))?;
                    config.id = AgentId::new(format!("approved-worker-{identity}"));
                    self.prepare_native_session_team_definition(
                        token,
                        config,
                        AgentDefinitionId::new(format!("worker-template-{identity}"))
                            .map_err(team_error)?,
                        1,
                        worker.limits.clone(),
                    )
                    .await?
                    .definition
                };
                let worker_writes = self.session_dispatch_lifecycles.with_session_team_stores(
                    token,
                    |_, content, _| match content
                        .resolve_activation_evidence(&definition.snapshot)
                        .map_err(team_error)?
                    {
                        ActivationEvidenceContent::Definition { profile, .. } => {
                            Ok(profile.write_scope.clone())
                        }
                        _ => Err(team_error("Approved Worker profile is missing")),
                    },
                )?;
                let coordinator_writes = checked_writes(slot, parent_config.writes.as_deref())?;
                if !axocoatl_session::path_scope::write_scope_within(
                    worker_writes.as_deref(),
                    coordinator_writes.as_deref(),
                ) {
                    return Err(team_error(format!(
                        "Worker {} may change {}, but {} may change only {}. A Worker cannot \
                         change more than its Coordinator: narrow the Worker's writes: in its \
                         configuration, or widen what {} may change",
                        worker.template_id,
                        describe_writes(worker_writes.as_deref()),
                        slot.name,
                        describe_writes(coordinator_writes.as_deref()),
                        slot.name
                    )));
                }
                workers.push(NativeCoordinatorWorker {
                    template_id: worker.template_id.clone(),
                    definition,
                    limits: worker.limits.clone(),
                    adhoc_allowed: worker.adhoc_allowed,
                });
            }
            approved.push((
                slot.slot_id.clone(),
                ApprovedCoordinatorPolicy {
                    workers,
                    operations: proposal.operations.clone(),
                    max_nodes: proposal.max_nodes,
                    max_edges: proposal.max_edges,
                    resource: ApprovedCoordinatorResource {
                        session_id: session.id.clone(),
                        workspace_id: session.workspace_id.clone(),
                        working_dir: session.working_dir.clone(),
                        environment_generation: session.environment.generation,
                        backend: self.config.sandbox.backend.clone(),
                        network: self.config.sandbox.network.clone(),
                        require_resource_limits: self.config.sandbox.require_resource_limits,
                        image: session.image.clone(),
                        setup_command: session.environment.setup_command.clone(),
                        setup_approved: session.environment.setup_approved,
                        setup_reviewed: session.environment.setup_reviewed,
                    },
                    legacy_htn_methods_yaml: None,
                },
            ));
        }
        Ok(approved)
    }

    /// The exact definition of the reviewer an edit names, captured like a
    /// helper template. Refused unless it is a read-only Worker whose budget
    /// covers every round.
    async fn prepare_review_approval(
        &self,
        session_id: &str,
        edit: &SessionTeamEdit,
        token: &super::session_dispatch::SessionTeamToken,
    ) -> Result<Option<ApprovedReview>, DaemonError> {
        let Some(review) = &edit.required_review else {
            return Ok(None);
        };
        let mut config = self
            .config
            .agents
            .iter()
            .find(|agent| agent.id == review.template_id)
            .ok_or_else(|| {
                team_error(format!(
                    "The reviewer template {} no longer exists",
                    review.template_id
                ))
            })?
            .to_core();
        if let Some(refusal) = review_refusal(&review.template_id, &config) {
            return Err(team_error(refusal));
        }
        let per_round = review_round_invocations(&config);
        let needed = per_round.saturating_mul(review.max_rounds);
        if review.limits.invocations < needed {
            let each = if per_round > 1 {
                "its answer and, because it has bash, the repository captures around it"
            } else {
                "its answer"
            };
            return Err(team_error(format!(
                "The reviewer {} needs at least {needed} invocations for {} rounds: {each} each \
                 round. Raise its invocation limit, or lower the review rounds",
                review.template_id, review.max_rounds
            )));
        }
        if review.max_output_tokens.is_some() {
            config.sampling.max_tokens = review.max_output_tokens;
        }
        let identity = digest(&(session_id, &edit.command_id, "required-review", review))?;
        config.id = AgentId::new(format!("approved-reviewer-{identity}"));
        let definition = self
            .prepare_native_session_team_definition(
                token,
                config,
                AgentDefinitionId::new(format!("review-template-{identity}"))
                    .map_err(team_error)?,
                1,
                review.limits.clone(),
            )
            .await?
            .definition;
        Ok(Some(ApprovedReview {
            template_id: review.template_id.clone(),
            definition,
            max_rounds: review.max_rounds,
            limits: review.limits.clone(),
        }))
    }

    async fn prepare_session_team_edit(
        &self,
        session_id: &str,
        edit: &SessionTeamEdit,
        apply_digest: Option<&str>,
    ) -> Result<SessionTeamPreview, DaemonError> {
        let command_id = CommandId::new(edit.command_id.clone()).map_err(team_error)?;
        if edit.slots.is_empty()
            || edit.slots.len() > MAX_CONTRACT_NODES
            || serde_json::to_vec(edit).map_err(team_error)?.len() > MAX_CONTRACT_ENVELOPE_BYTES
        {
            return Err(team_error(
                "Session team exceeds the supported graph bounds",
            ));
        }
        check_required_checks(edit, 0)?;
        check_required_review(edit)?;
        let token = self
            .session_dispatch_lifecycles
            .session_team_token(session_id)?;
        // A durable exact Apply is its original receipt even if templates,
        // provider availability, or grant expiry changed after admission.
        let repeated = self.session_dispatch_lifecycles.with_session_team_stores(
            &token,
            |canonical, content, _| {
                let store = SessionTeamStore::open_owned(
                    canonical
                        .component_namespace(ExecutionComponent::SessionTeam)
                        .map_err(team_error)?,
                    canonical,
                    content,
                    None,
                )
                .map_err(team_error)?;
                for revision in 1..=store.configuration_revision().map_err(team_error)? {
                    let Some(record) = store.get(revision).map_err(team_error)? else {
                        return Err(team_error("Saved Session configuration is missing"));
                    };
                    if record.command_id != command_id {
                        continue;
                    }
                    let first = record
                        .graph
                        .slots
                        .first()
                        .ok_or_else(|| team_error("Saved Session team is empty"))?;
                    let approval = approval_for_slot(content, first)?
                        .ok_or_else(|| team_error("Saved Apply body is unavailable"))?;
                    if approval.edit != *edit {
                        return Err(team_error(
                            "This Apply command already has a different body",
                        ));
                    }
                    let commit = SessionTeamCommit {
                        schema_version: record.schema_version,
                        command_id: record.command_id.clone(),
                        expected_configuration_revision: record.expected_configuration_revision,
                        graph: record.graph.clone(),
                        initial_source: record.initial_source.clone(),
                        continuity: record.continuity.clone(),
                        layout: record.layout.clone(),
                    };
                    let review_digest = digest(&(edit, &commit))?;
                    if apply_digest.is_some_and(|supplied| supplied != review_digest) {
                        return Err(team_error(
                            "This Apply review differs from the saved command",
                        ));
                    }
                    let changes = record
                        .continuity
                        .iter()
                        .map(|continuity| SessionTeamChange {
                            slot_id: continuity.slot_id.as_str().into(),
                            kind: "saved".into(),
                            history: match continuity.decision {
                                SessionTeamContinuity::PreserveUnchanged => "preserved",
                                _ => "new conversation",
                            }
                            .into(),
                        })
                        .collect();
                    return Ok(Some(SessionTeamPreview {
                        edit: edit.clone(),
                        review_digest,
                        changes,
                        configuration_revision: record.configuration_revision,
                        applies_to: "future_turns",
                        profiles: review_profiles(content, &record.graph)?,
                        coordinators: approval.coordinators,
                    }));
                }
                if store.configuration_revision().map_err(team_error)?
                    != edit.expected_configuration_revision
                {
                    return Err(team_error(
                        "Session configuration changed; refresh and review your edits again",
                    ));
                }
                Ok(None)
            },
        )?;
        if let Some(receipt) = repeated {
            return Ok(receipt);
        }
        let previous = self.session_dispatch_lifecycles.with_session_team_stores(
            &token,
            |canonical, content, _| {
                let store = SessionTeamStore::open_owned(
                    canonical
                        .component_namespace(ExecutionComponent::SessionTeam)
                        .map_err(team_error)?,
                    canonical,
                    content,
                    None,
                )
                .map_err(team_error)?;
                // Retrieve the exact predecessor even for an idempotent Apply retry.
                if edit.expected_configuration_revision == 0 {
                    Ok(None)
                } else {
                    store
                        .get(edit.expected_configuration_revision)
                        .map_err(team_error)?
                        .cloned()
                        .map(Some)
                        .ok_or_else(|| {
                            team_error("Session configuration changed; refresh the team")
                        })
                }
            },
        )?;
        let templates = edit
            .slots
            .iter()
            .map(|proposed| {
                let template = match &proposed.template_id {
                    Some(id) => {
                        if !self.config.agents.iter().any(|agent| &agent.id == id) {
                            return Err(team_error("Selected Agent template no longer exists"));
                        }
                        Some(id.clone())
                    }
                    None => {
                        let source = previous.as_ref().and_then(|revision| {
                            revision.graph.slots.iter().find(|slot| {
                                slot.slot_id.as_str() == proposed.slot_id
                                    || proposed.source_slot_id.as_deref()
                                        == Some(slot.slot_id.as_str())
                            })
                        });
                        source
                            .map(|slot| {
                                self.session_dispatch_lifecycles
                                    .with_session_team_stores(&token, |_, content, _| {
                                        approved_template_for_slot(content, slot)
                                    })
                            })
                            .transpose()?
                            .flatten()
                    }
                };
                Ok((proposed.slot_id.clone(), template))
            })
            .collect::<Result<Vec<_>, DaemonError>>()?;
        let coordinators = self
            .prepare_coordinator_approvals(session_id, edit, previous.as_ref(), &token)
            .await?;
        let review = self
            .prepare_review_approval(session_id, edit, &token)
            .await?;
        let approval = serde_json::to_string(&SessionTeamApproval {
            kind: "authenticated_session_team_apply".into(),
            edit: edit.clone(),
            templates,
            coordinators: coordinators.clone(),
            review,
        })
        .map_err(team_error)?;
        let mut slots = Vec::new();
        let mut continuity = Vec::new();
        let mut changes = Vec::new();
        let mut ids = HashSet::new();
        // The first required Agent with bash, which pays for the required
        // checks of a turn sent to the whole team, and its invocation limit.
        let mut shell_payer: Option<(String, u32)> = None;
        for proposed in &edit.slots {
            let slot_id = SessionTeamSlotId::new(proposed.slot_id.clone()).map_err(team_error)?;
            if !ids.insert(slot_id.clone()) {
                return Err(team_error("Team slot identities must be unique"));
            }
            let limits = proposed.limits.clone().ok_or_else(|| {
                team_error(
                    "Enter explicit activation, invocation, token and cost limits for every Agent",
                )
            })?;
            let expires_at_ms = proposed
                .expires_at_ms
                .filter(|value| {
                    *value
                        > std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|time| time.as_millis() as u64)
                            .unwrap_or(u64::MAX)
                })
                .ok_or_else(|| team_error("Enter an explicit budget expiry for every Agent"))?;
            if limits.activations == 0 || limits.invocations == 0 || limits.tokens == 0 {
                return Err(team_error(
                    "Activation, invocation and token limits must be positive",
                ));
            }
            let prior = previous.as_ref().and_then(|revision| {
                revision
                    .graph
                    .slots
                    .iter()
                    .find(|slot| slot.slot_id == slot_id)
            });
            let config_source = prior.or_else(|| {
                proposed.source_slot_id.as_ref().and_then(|id| {
                    previous
                        .as_ref()?
                        .graph
                        .slots
                        .iter()
                        .find(|slot| slot.slot_id.as_str() == id)
                })
            });
            let old_config = config_source
                .map(|slot| {
                    self.session_dispatch_lifecycles.with_session_team_stores(
                        &token,
                        |_, content, _| match content
                            .resolve_activation_evidence(&slot.definition.snapshot)
                            .map_err(team_error)?
                        {
                            ActivationEvidenceContent::Definition { configuration, .. } => {
                                serde_json::from_str::<AgentConfig>(configuration)
                                    .map_err(team_error)
                            }
                            _ => Err(team_error("Session definition is unavailable")),
                        },
                    )
                })
                .transpose()?;
            let mut config = match &proposed.template_id {
                Some(id) => self
                    .config
                    .agents
                    .iter()
                    .find(|agent| &agent.id == id)
                    .ok_or_else(|| team_error("Selected Agent template no longer exists"))?
                    .to_core(),
                None => old_config
                    .clone()
                    .ok_or_else(|| team_error("Choose an Agent template for each new slot"))?,
            };
            config.name = proposed.name.clone();
            config.provider = proposed.provider.clone();
            config.model = proposed.model.clone();
            config.system_prompt = proposed.instructions.clone();
            config.sampling.max_tokens = proposed.max_output_tokens;
            config.writes = checked_writes(proposed, config.writes.as_deref())?;
            if let Some(old) = &old_config {
                config.id = old.id.clone();
            }
            if shell_payer.is_none()
                && proposed.required
                && config.tools.iter().any(|tool| tool == "bash")
            {
                shell_payer = Some((proposed.name.clone(), limits.invocations));
            }
            let unchanged = prior.is_some()
                && !proposed.reset_history
                && old_config.as_ref().is_some_and(|old| {
                    serde_json::to_value(old).ok() == serde_json::to_value(&config).ok()
                });
            let identity = digest(&(session_id, &edit.command_id, &slot_id))?;
            let conversation_id = if unchanged {
                prior
                    .ok_or_else(|| team_error("Preserved conversation has no predecessor"))?
                    .conversation_id
                    .clone()
            } else {
                NodeConversationId::new(format!("team-conversation-{identity}"))
                    .map_err(team_error)?
            };
            config.id = AgentId::new(conversation_id.as_str());
            let node_id = prior.map(|slot| slot.node_id.clone()).unwrap_or(
                TurnNodeId::new(format!(
                    "team-node-{}",
                    &digest(&(session_id, &slot_id))?[..24]
                ))
                .map_err(team_error)?,
            );
            let captured = if unchanged {
                None
            } else {
                let definition_id = AgentDefinitionId::new(format!("team-definition-{identity}"))
                    .map_err(team_error)?;
                Some(
                    self.prepare_native_session_team_definition(
                        &token,
                        config,
                        definition_id,
                        1,
                        limits.clone(),
                    )
                    .await?,
                )
            };
            let definition = captured
                .as_ref()
                .map(|value| value.definition.clone())
                .or_else(|| prior.map(|slot| slot.definition.clone()))
                .ok_or_else(|| team_error("Session definition is missing"))?;
            let (budget, grant) = self.session_dispatch_lifecycles.with_session_team_stores(
                &token,
                |_, content, _| {
                    let profile = match content
                        .resolve_activation_evidence(&definition.snapshot)
                        .map_err(team_error)?
                    {
                        ActivationEvidenceContent::Definition { profile, .. } => profile.clone(),
                        _ => return Err(team_error("Definition profile missing")),
                    };
                    let budget = content
                        .retain_activation_evidence(ActivationEvidenceContent::Budget {
                            limits: limits.clone(),
                        })
                        .map_err(team_error)?
                        .reference()
                        .clone();
                    let issuer = content
                        .retain_activation_evidence(ActivationEvidenceContent::Guidance {
                            text: approval.clone(),
                        })
                        .map_err(team_error)?
                        .reference()
                        .clone();
                    let mut profiles = vec![profile];
                    if let Some((_, approved)) = coordinators
                        .iter()
                        .find(|(slot, _)| slot == &proposed.slot_id)
                    {
                        for worker in &approved.workers {
                            let ActivationEvidenceContent::Definition { profile, .. } = content
                                .resolve_activation_evidence(&worker.definition.snapshot)
                                .map_err(team_error)?
                            else {
                                return Err(team_error("Approved Worker profile is missing"));
                            };
                            if !profiles.contains(profile) {
                                profiles.push(profile.clone());
                            }
                        }
                    }
                    let policy = AuthorityGrant {
                        id: format!("team-grant-{identity}"),
                        revision: 1,
                        issuer_evidence: issuer,
                        holder: node_id.clone(),
                        descendants: vec![],
                        allow_stop_descendants: false,
                        delegation: None,
                        profiles,
                        conditions: vec![],
                        limits: limits.clone(),
                        expires_at_ms,
                    };
                    let grant = content
                        .retain_activation_evidence(ActivationEvidenceContent::Grant { policy })
                        .map_err(team_error)?
                        .reference()
                        .clone();
                    Ok((budget, grant))
                },
            )?;
            changes.push(SessionTeamChange {
                slot_id: proposed.slot_id.clone(),
                kind: if prior.is_none() {
                    "added"
                } else if unchanged {
                    "retained"
                } else {
                    "replaced"
                }
                .into(),
                history: if unchanged {
                    "preserved"
                } else {
                    "new conversation"
                }
                .into(),
            });
            continuity.push(SlotContinuityDecision {
                slot_id: slot_id.clone(),
                decision: if unchanged {
                    SessionTeamContinuity::PreserveUnchanged
                } else {
                    SessionTeamContinuity::Reset
                },
            });
            slots.push(SessionTeamSlot {
                slot_id,
                node_id,
                definition,
                conversation_id,
                required: proposed.required,
                budget,
                grant: Some(grant),
            });
        }
        if let Some(previous) = &previous {
            for removed in previous
                .graph
                .slots
                .iter()
                .filter(|slot| !ids.contains(&slot.slot_id))
            {
                changes.push(SessionTeamChange {
                    slot_id: removed.slot_id.as_str().into(),
                    kind: "removed".into(),
                    history: "retained in history".into(),
                });
            }
        }
        if !edit.required_checks.is_empty() {
            let Some((name, limit)) = &shell_payer else {
                return Err(team_error(
                    "Required checks run on the allowance of a required Agent that has the bash \
                     tool, and no required Agent in this team has bash. Make an Agent with bash \
                     required, or remove the required checks",
                ));
            };
            let checks = edit.required_checks.len();
            let rounds = edit
                .required_review
                .as_ref()
                .map_or(1, |review| review.max_rounds);
            let minimum = axocoatl_session::turn_review::payer_minimum_invocations(checks, rounds);
            if *limit < minimum && rounds > 1 {
                return Err(team_error(format!(
                    "{name} runs the required checks on its budget, and the required review can \
                     send its result back {} times, so its invocation limit must be at least \
                     {minimum}: the checks and the repository captures around them for every \
                     round, and its own captures and answer each time it runs. Raise its \
                     invocation limit, lower the review rounds, or remove required checks",
                    rounds - 1
                )));
            }
            if *limit < minimum {
                return Err(team_error(format!(
                    "{name} runs the required checks on its budget, so its invocation limit must \
                     be at least {minimum}: {} to run the checks and the repository captures \
                     around them twice (after the Agents finish, and once more if you \
                     Continue), 2 to capture its own changes and 1 for its answer. Raise its \
                     invocation limit, or remove required checks",
                    axocoatl_session::turn_checks::check_allowance(checks)
                )));
            }
        }
        if let Some(review) = &edit.required_review {
            // Every round that asks for changes runs the lead again: each
            // required Agent no other required Agent waits for.
            let required = |id: &str| {
                edit.slots
                    .iter()
                    .any(|slot| slot.slot_id == id && slot.required)
            };
            for slot in edit.slots.iter().filter(|slot| {
                slot.required
                    && !edit
                        .dependencies
                        .iter()
                        .any(|edge| edge.parent == slot.slot_id && required(&edge.child))
            }) {
                let activations = slot.limits.as_ref().map_or(0, |limits| limits.activations);
                if activations < review.max_rounds {
                    return Err(team_error(format!(
                        "The required review can run {} {} times in one turn, once per round, so \
                         its activation limit must be at least {}. Raise it, or lower the \
                         review rounds",
                        slot.name, review.max_rounds, review.max_rounds
                    )));
                }
            }
        }
        let dependencies = edit
            .dependencies
            .iter()
            .map(|edge| {
                let find = |id: &str| {
                    slots
                        .iter()
                        .find(|slot| slot.slot_id.as_str() == id)
                        .map(|slot| slot.node_id.clone())
                        .ok_or_else(|| team_error("Dependency references an unselected Agent"))
                };
                Ok(DependencyEdge {
                    parent: find(&edge.parent)?,
                    child: find(&edge.child)?,
                })
            })
            .collect::<Result<Vec<_>, DaemonError>>()?;
        if let Some(previous) = &previous {
            check_required_checks(edit, previous.graph.conditions.len())?;
        }
        let commit = SessionTeamCommit {
            schema_version: SESSION_TEAM_SCHEMA_VERSION,
            command_id,
            expected_configuration_revision: edit.expected_configuration_revision,
            graph: SessionTeamGraph {
                slots,
                dependencies,
                conditions: previous
                    .as_ref()
                    .map(|revision| revision.graph.conditions.clone())
                    .unwrap_or_default(),
            },
            initial_source: None,
            continuity,
            layout: edit.layout.clone(),
        };
        let profiles = self
            .session_dispatch_lifecycles
            .with_session_team_stores(&token, |_, content, _| {
                review_profiles(content, &commit.graph)
            })?;
        let review_digest = digest(&(edit, &commit))?;
        if apply_digest.is_some_and(|supplied| supplied != review_digest) {
            return Err(team_error(
                "Team review changed; Preview again before Apply",
            ));
        }
        let configuration_revision = self.session_dispatch_lifecycles.with_session_team_stores(
            &token,
            |canonical, content, _| {
                let mut store = SessionTeamStore::open_owned(
                    canonical
                        .component_namespace(ExecutionComponent::SessionTeam)
                        .map_err(team_error)?,
                    canonical,
                    content,
                    None,
                )
                .map_err(team_error)?;
                let revision = if apply_digest.is_some() {
                    store.commit(commit, canonical, content, None)
                } else {
                    store.preview(commit, canonical, content, None)
                }
                .map_err(team_error)?;
                Ok(revision.configuration_revision)
            },
        )?;
        Ok(SessionTeamPreview {
            edit: edit.clone(),
            review_digest,
            changes,
            configuration_revision,
            applies_to: "future_turns",
            profiles,
            coordinators,
        })
    }
    pub async fn preview_session_team(
        &self,
        id: &str,
        edit: SessionTeamEdit,
    ) -> Result<SessionTeamPreview, DaemonError> {
        self.prepare_session_team_edit(id, &edit, None).await
    }
    pub async fn apply_session_team(
        &self,
        id: &str,
        request: SessionTeamApply,
    ) -> Result<SessionTeamPreview, DaemonError> {
        self.prepare_session_team_edit(id, &request.edit, Some(&request.review_digest))
            .await
    }
    pub fn cancel_session_team(
        &self,
        id: &str,
        request: SessionTeamCancel,
    ) -> Result<(), DaemonError> {
        CommandId::new(request.command_id).map_err(team_error)?;
        // Drafts carry no executable authority and never advance the team revision.
        self.session_dispatch_lifecycles
            .session_team_token(id)
            .map(|_| ())
    }
}

#[cfg(test)]
mod writes_tests {
    use super::*;

    /// A slot edit as a client sends it; `None` leaves `writes` out.
    fn slot(writes: Option<serde_json::Value>) -> SessionTeamSlotEdit {
        let mut value = serde_json::json!({
            "slot_id": "slot", "template_id": null, "source_slot_id": null,
            "name": "Helper", "provider": "ollama", "model": "model",
            "instructions": null, "max_output_tokens": null, "required": true,
            "reset_history": false, "limits": null, "expires_at_ms": null
        });
        if let Some(writes) = writes {
            value["writes"] = writes;
        }
        serde_json::from_value(value).unwrap()
    }

    /// A team without a required review keeps the exact bytes of its edit
    /// and of the approval every grant is issued from, so older Applies load
    /// and digest as before. Rounds default to two and are checked with the
    /// reviewer's budget before anything is prepared.
    #[test]
    fn edits_and_approvals_without_a_review_keep_their_exact_bytes() {
        let edit = r#"{"command_id":"apply","expected_configuration_revision":1,"slots":[],"dependencies":[],"layout":[],"required_checks":[["cargo","test"]]}"#;
        let parsed: SessionTeamEdit = serde_json::from_str(edit).unwrap();
        assert_eq!(parsed.required_review, None);
        assert_eq!(serde_json::to_string(&parsed).unwrap(), edit);
        check_required_review(&parsed).unwrap();
        let approval = format!(
            r#"{{"kind":"authenticated_session_team_apply","edit":{edit},"templates":[["slot","coder"]]}}"#
        );
        let parsed: SessionTeamApproval = serde_json::from_str(&approval).unwrap();
        assert_eq!(parsed.review, None);
        assert_eq!(serde_json::to_string(&parsed).unwrap(), approval);

        let setting: ReviewSetting = serde_json::from_str(
            r#"{"template_id":"reviewer","limits":{"activations":2,"invocations":2,"tokens":1,"cost_microunits":0}}"#,
        )
        .unwrap();
        assert_eq!(setting.max_rounds, 2);
        let mut edit: SessionTeamEdit = serde_json::from_str(edit).unwrap();
        edit.required_review = Some(setting.clone());
        check_required_review(&edit).unwrap();
        for (rounds, activations) in [(0, 2), (4, 4), (2, 1)] {
            let mut refused = edit.clone();
            let review = refused.required_review.as_mut().unwrap();
            review.max_rounds = rounds;
            review.limits.activations = activations;
            assert!(
                check_required_review(&refused).is_err(),
                "{rounds} {activations}"
            );
        }
    }

    /// Leaving `writes` out keeps the scope the slot starts from; `null`
    /// lets it change any file; a list is checked and replaces it.
    #[test]
    fn omitted_writes_keep_the_base_and_never_widen_it() {
        let base = vec!["lib/".to_owned()];
        let absent = slot(None);
        assert_eq!(absent.writes, None);
        assert_eq!(
            checked_writes(&absent, Some(&base)).unwrap(),
            Some(base.clone())
        );
        assert_eq!(checked_writes(&absent, Some(&[])).unwrap(), Some(vec![]));
        assert_eq!(checked_writes(&absent, None).unwrap(), None);
        let any = slot(Some(serde_json::Value::Null));
        assert_eq!(any.writes, Some(None));
        assert_eq!(checked_writes(&any, Some(&base)).unwrap(), None);
        let read_only = slot(Some(serde_json::json!([])));
        assert_eq!(checked_writes(&read_only, None).unwrap(), Some(vec![]));
        assert!(checked_writes(&slot(Some(serde_json::json!([".git/"]))), None).is_err());
        // Absent stays absent and null stays null when written back.
        let written = serde_json::to_value(&absent).unwrap();
        assert!(written.get("writes").is_none(), "{written}");
        let written = serde_json::to_value(&any).unwrap();
        assert_eq!(written.get("writes"), Some(&serde_json::Value::Null));
    }
}

#[cfg(test)]
mod legacy_approval_tests {
    use super::*;

    /// A Coordinator approval retained by an earlier build may carry HTN
    /// methods. It still parses, and approvals keep writing the key as null.
    #[test]
    fn approval_with_removed_htn_methods_still_parses() {
        let mut value = serde_json::json!({
            "workers": [], "operations": [], "max_nodes": 1, "max_edges": 0,
            "resource": {
                "session_id": "session", "workspace_id": "workspace",
                "working_dir": "/work", "environment_generation": 1,
                "backend": "podman", "network": "none",
                "require_resource_limits": false, "image": null,
                "setup_command": null, "setup_approved": false, "setup_reviewed": true
            },
            "htn_methods_yaml": "- task_pattern: \"Do the work\"\n  subtasks: []\n"
        });
        let mut policy: ApprovedCoordinatorPolicy = serde_json::from_value(value.clone()).unwrap();
        policy.legacy_htn_methods_yaml = None;
        value["htn_methods_yaml"] = serde_json::Value::Null;
        assert_eq!(serde_json::to_value(&policy).unwrap(), value);
    }
}
