//! Transport-free, read-only execution evidence for the Session workbench.
//!
//! This projection does not grant execution authority. Legacy identities remain
//! legacy identities, and missing bodies never become empty successful results.
//! Callers construct it while holding the owning history/controller read lock.

use axocoatl_session::control_command::CommandReceiptView;
use axocoatl_session::execution_content::{
    ActivationEvidenceContent, ContentResolution, ExecutionContentError, ExecutionContentStore,
};
use axocoatl_session::execution_store::DurableTurnSnapshot;
use axocoatl_session::turn_contract::{ActivationRef, ActivationState, LogicalTurnState};
use axocoatl_session::turn_ledger::{
    SessionTurn, SessionTurnAgentOutputDisposition, SessionTurnLifecycle,
};
use serde::Serialize;
use serde_json::{json, Map, Value};

const TEXT_PREVIEW_BYTES: usize = 64 * 1024;

/// Availability is part of the wire contract. Unknown is distinct from zero;
/// missing is distinct from not recorded, and retained previews declare loss.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum EvidenceValue<T> {
    Available { value: T },
    NotRecorded,
    Unknown { reason: String },
    Missing { reference: String },
    Unavailable { reason: String },
    Truncated { value: T, original_byte_len: u64 },
}

impl<T> EvidenceValue<T> {
    fn available(value: T) -> Self {
        Self::Available { value }
    }

    fn from_option(value: Option<T>) -> Self {
        value.map_or(Self::NotRecorded, Self::available)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ControlPlaneActivationRef {
    Legacy {
        session_id: String,
        turn_id: String,
        node_id: String,
        generation: Option<u32>,
    },
    Exact {
        activation: ActivationRef,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ControlPlaneCapability {
    pub enabled: bool,
    /// This is a request affordance, not live authority. Submission must attach
    /// the actual runtime and pass the unchanged canonical command validator.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub requires_revalidation: bool,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HumanResponseCapability {
    pub blocker_id: axocoatl_session::turn_contract::BlockerId,
    pub request: axocoatl_session::turn_contract::EvidenceRef,
    pub state: Value,
    pub display: Value,
    pub approve: ControlPlaneCapability,
    pub decline: ControlPlaneCapability,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ControlPlaneCapabilities {
    pub inspect: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub human_responses: Vec<HumanResponseCapability>,
    pub stop: ControlPlaneCapability,
    pub retry: ControlPlaneCapability,
    pub guide: ControlPlaneCapability,
    pub revise: ControlPlaneCapability,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub revise_invalidates: Vec<ActivationRef>,
}

impl ControlPlaneCapabilities {
    fn read_only(reason: &str) -> Self {
        let unavailable = || ControlPlaneCapability {
            requires_revalidation: false,
            enabled: false,
            reason: reason.into(),
        };
        Self {
            inspect: true,
            human_responses: vec![],
            stop: unavailable(),
            retry: unavailable(),
            guide: unavailable(),
            revise: unavailable(),
            revise_invalidates: vec![],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ControlPlaneDefinition {
    pub name: EvidenceValue<String>,
    pub role: EvidenceValue<String>,
    pub provider: EvidenceValue<String>,
    pub model: EvidenceValue<String>,
    pub instructions: EvidenceValue<String>,
    pub tools: EvidenceValue<Vec<String>>,
    pub configuration_revision: EvidenceValue<u64>,
    pub snapshot: EvidenceValue<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ControlPlaneEvidence {
    pub kind: String,
    pub reference: EvidenceValue<String>,
    pub summary: EvidenceValue<String>,
    pub recorded_at: EvidenceValue<u64>,
    pub details: EvidenceValue<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ControlPlaneOutput {
    pub text: String,
    pub truncated: bool,
    pub original_byte_len: EvidenceValue<u64>,
    pub reference: EvidenceValue<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ControlPlaneActivation {
    pub reference: ControlPlaneActivationRef,
    pub generation: EvidenceValue<u32>,
    pub state: String,
    pub reason: EvidenceValue<String>,
    /// Milliseconds since Unix epoch, only when actually recorded.
    pub started_at: EvidenceValue<u64>,
    pub completed_at: EvidenceValue<u64>,
    pub input: EvidenceValue<Value>,
    pub output: EvidenceValue<String>,
    pub partial_outputs: Vec<ControlPlaneOutput>,
    pub usage: EvidenceValue<Value>,
    pub capabilities: ControlPlaneCapabilities,
    pub evidence: Vec<ControlPlaneEvidence>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ControlPlaneNode {
    pub node_id: String,
    pub definition_id: String,
    pub label: String,
    pub definition: EvidenceValue<ControlPlaneDefinition>,
    pub dependencies: Vec<String>,
    pub activations: Vec<ControlPlaneActivation>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ControlPlaneEdge {
    pub id: String,
    pub kind: String,
    pub source: String,
    pub target: String,
    pub generation: EvidenceValue<u32>,
    pub recorded_at: EvidenceValue<u64>,
    pub summary: EvidenceValue<String>,
    pub evidence: EvidenceValue<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SessionTurnControlPlane {
    pub schema_version: u32,
    pub history_version: String,
    /// Conversation rewind visibility is independent of historical activation acceptance.
    pub superseded_conversation: bool,
    pub session_id: String,
    pub turn_id: String,
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_requested: Option<axocoatl_session::turn_contract::TurnStopIntent>,
    pub turn_revision: EvidenceValue<u64>,
    pub graph_revision: EvidenceValue<u64>,
    pub request: EvidenceValue<String>,
    pub nodes: Vec<ControlPlaneNode>,
    pub edges: Vec<ControlPlaneEdge>,
    pub epochs: EvidenceValue<Vec<Value>>,
    pub accepted_inputs: EvidenceValue<Vec<Value>>,
    pub invocations: EvidenceValue<Vec<Value>>,
    pub conditions: EvidenceValue<Vec<Value>>,
    pub commands: EvidenceValue<Vec<CommandReceiptView>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_controls: Option<crate::session_dispatch::HumanTurnControls>,
    pub decisions: EvidenceValue<Vec<Value>>,
    pub warnings: Vec<String>,
}

impl SessionTurnControlPlane {
    pub(crate) fn mark_conversation_superseded(&mut self, superseded: bool) {
        self.superseded_conversation |= superseded;
        let warning="This turn was superseded by a conversation rewind. Its historical evidence remains attachable; recorded execution acceptance and external effects are unchanged.";
        if self.superseded_conversation && !self.warnings.iter().any(|value| value == warning) {
            self.warnings.push(warning.into());
        }
    }
    /// Project only historical data. Current Agent settings are deliberately not
    /// an input: their values cannot establish what an old activation consumed.
    pub fn from_legacy(turn: &SessionTurn) -> Self {
        let mut view = Self {
            schema_version: 1,
            turn_controls: None,
            history_version: "legacy_v1".into(),
            superseded_conversation: false,
            stop_requested: None,
            session_id: turn.session_id.clone(),
            turn_id: turn.id.clone(),
            state: json!(turn.status).as_str().unwrap_or("unknown").into(),
            turn_revision: EvidenceValue::NotRecorded,
            graph_revision: EvidenceValue::NotRecorded,
            request: bounded_text(&turn.user_input),
            nodes: vec![],
            edges: vec![],
            epochs: EvidenceValue::NotRecorded,
            accepted_inputs: EvidenceValue::NotRecorded,
            invocations: EvidenceValue::Unknown {
                reason: "This history does not record a complete invocation intent/outcome audit.".into(),
            },
            conditions: EvidenceValue::NotRecorded,
            commands: EvidenceValue::NotRecorded,
            decisions: EvidenceValue::NotRecorded,
            warnings: vec!["Legacy history does not record exact execution epochs, activation IDs, or complete definition snapshots.".into()],
        };
        view.mark_conversation_superseded(turn.superseded);
        for record in &turn.execution_events {
            let metadata = &record.event.metadata;
            // A corrupted/misrouted metadata envelope cannot cross Sessions or
            // turns just because it was included in a presentation collection.
            if text(metadata, &["session_id"]).is_some_and(|id| id != turn.session_id)
                || text(metadata, &["turn_id"]).is_some_and(|id| id != turn.id)
            {
                view.warnings
                    .push(format!("Ignored mismatched event {}.", record.operation_id));
                continue;
            }
            let kind = record.event.kind.as_str();
            if kind == "coordination_planned" {
                if let Some(agents) = metadata.get("agents").and_then(Value::as_array) {
                    for agent in agents {
                        let object = agent.as_object();
                        let id = agent
                            .as_str()
                            .or_else(|| object.and_then(|a| text(a, &["id", "agent_id"])));
                        let Some(id) = id else { continue };
                        let node = legacy_node(&mut view.nodes, id);
                        if let Some(name) = object.and_then(|a| text(a, &["name", "label"])) {
                            node.label = name.into();
                        }
                        if let Some(dependencies) = object.and_then(|a| a.get("depends_on")) {
                            node.dependencies = string_list(dependencies);
                        }
                    }
                }
                // The current runtime writes dependencies on each planned node.
                // Older explicit edge arrays are accepted only with both ends.
                for key in ["dependencies", "edges"] {
                    if let Some(edges) = metadata.get(key).and_then(Value::as_array) {
                        for edge in edges.iter().filter_map(Value::as_object) {
                            if let (Some(parent), Some(child)) = (
                                text(edge, &["from", "source", "parent"]),
                                text(edge, &["to", "target", "child"]),
                            ) {
                                let node = legacy_node(&mut view.nodes, child);
                                if !node.dependencies.iter().any(|id| id == parent) {
                                    node.dependencies.push(parent.into());
                                }
                            }
                        }
                    }
                }
            }
            let generation = generation(metadata);
            if let Some(state) = legacy_event_state(kind) {
                if let Some(id) = text(metadata, &["agent_id", "agent"]) {
                    let node = legacy_node(&mut view.nodes, id);
                    let activation = legacy_activation(node, turn, generation);
                    activation.state = state.into();
                    activation.reason = text_value(metadata, &["reason", "summary", "error"]);
                    if kind == "coordination_agent_activated" {
                        activation.started_at = EvidenceValue::available(record.recorded_at);
                    } else if matches!(state, "completed" | "failed" | "cancelled") {
                        activation.completed_at = EvidenceValue::available(record.recorded_at);
                    }
                    if let Some(usage) = metadata.get("usage") {
                        activation.usage = EvidenceValue::available(usage.clone());
                    }
                    activation.evidence.push(ControlPlaneEvidence {
                        kind: kind.into(),
                        reference: EvidenceValue::available(record.operation_id.clone()),
                        summary: text_value(metadata, &["summary", "reason", "error"]),
                        recorded_at: EvidenceValue::available(record.recorded_at),
                        details: EvidenceValue::available(selected_metadata(
                            metadata,
                            &["parents", "cause_signal_ids", "signal_id"],
                        )),
                    });
                }
            }
            if matches!(kind, "tool_started" | "tool_result") {
                if let Some(id) = text(metadata, &["agent_id", "agent"]) {
                    let node = legacy_node(&mut view.nodes, id);
                    let activation = legacy_activation(node, turn, generation);
                    activation.evidence.push(ControlPlaneEvidence {
                        kind: kind.into(),
                        reference: EvidenceValue::available(record.operation_id.clone()),
                        summary: text_value(metadata, &["name", "tool_name", "tool"]),
                        recorded_at: EvidenceValue::available(record.recorded_at),
                        details: bounded_json(selected_metadata(
                            metadata,
                            &[
                                "id",
                                "call_id",
                                "name",
                                "tool_name",
                                "arguments",
                                "result",
                                "is_error",
                                "occurrence",
                                "truncated",
                                "original_byte_len",
                            ],
                        )),
                    });
                }
            }
            if kind == "coordination_signal" {
                if let (Some(source), Some(target)) = (
                    text(metadata, &["from_agent", "from", "source"]),
                    text(metadata, &["to_agent", "to", "target"]),
                ) {
                    let applied = metadata.get("applied").and_then(Value::as_bool);
                    view.edges.push(ControlPlaneEdge {
                        id: text(metadata, &["signal_id"])
                            .unwrap_or(&record.operation_id)
                            .into(),
                        kind: if applied == Some(false) {
                            "unapplied_signal"
                        } else {
                            "signal"
                        }
                        .into(),
                        source: source.into(),
                        target: target.into(),
                        generation: EvidenceValue::from_option(generation),
                        recorded_at: EvidenceValue::available(record.recorded_at),
                        summary: text_value(metadata, &["summary", "message"]),
                        evidence: EvidenceValue::available(record.operation_id.clone()),
                    });
                }
            }
        }
        for output in &turn.agent_outputs {
            let node = legacy_node(&mut view.nodes, &output.agent_id);
            let activation = legacy_activation(node, turn, output.activation_generation);
            // A superseded result is historical evidence, never the current
            // answer. The disposition does not replace a later blocked state.
            if output.superseded {
                activation.state = "superseded".into();
            } else if activation.state == "unknown" {
                activation.state = match output.disposition {
                    Some(SessionTurnAgentOutputDisposition::Completed) => "completed",
                    Some(SessionTurnAgentOutputDisposition::ChangesRequested) => {
                        "changes_requested"
                    }
                    Some(SessionTurnAgentOutputDisposition::Failed) => "failed",
                    Some(SessionTurnAgentOutputDisposition::Cancelled) => "cancelled",
                    None => "unknown",
                }
                .into();
            }
            activation.output = bounded_text(&output.output);
            activation.evidence.push(ControlPlaneEvidence {
                kind: "agent_output".into(),
                reference: EvidenceValue::from_option(output.operation_id.clone()),
                summary: bounded_text(&output.output),
                recorded_at: EvidenceValue::available(output.recorded_at),
                details: EvidenceValue::available(json!({
                    "model": output.model,
                    "disposition": output.disposition,
                    "superseded": output.superseded,
                    "superseded_by_generation": output.superseded_by_generation,
                    "causal_signal_id": output.causal_signal_id,
                })),
            });
        }
        // A direct turn's selected Agent is recorded, but the legacy ledger
        // cannot manufacture a generation/epoch or an exact definition snapshot.
        if let Some(id) = &turn.agent_id {
            let node = legacy_node(&mut view.nodes, id);
            if node.activations.is_empty() {
                let activation = legacy_activation(node, turn, None);
                activation.state = view.state.clone();
                activation.reason = EvidenceValue::from_option(turn.error.clone());
                activation.output = turn
                    .final_output
                    .as_deref()
                    .map_or(EvidenceValue::NotRecorded, bounded_text);
                if !turn.partial_output.is_empty() {
                    activation.partial_outputs.push(partial_output(
                        &turn.partial_output,
                        None,
                        None,
                    ));
                }
                // Turn creation is not evidence of the Agent's actual start.
                activation.completed_at = EvidenceValue::from_option(turn.completed_at);
            }
        }
        if turn.status.is_terminal() {
            for node in &mut view.nodes {
                for activation in &mut node.activations {
                    if matches!(activation.state.as_str(), "running" | "waiting") {
                        activation.state = match turn.status {
                            SessionTurnLifecycle::Interrupted => "interrupted",
                            SessionTurnLifecycle::Cancelled => "cancelled",
                            _ => "unknown",
                        }
                        .into();
                        activation.reason = EvidenceValue::available(
                            "The turn closed without a recorded terminal event for this activation.".into()
                        );
                    }
                }
            }
        }
        for node in &view.nodes {
            for parent in &node.dependencies {
                view.edges.push(ControlPlaneEdge {
                    id: format!("legacy-dependency:{}:{}", parent, node.node_id),
                    kind: "dependency".into(),
                    source: parent.clone(),
                    target: node.node_id.clone(),
                    generation: EvidenceValue::NotRecorded,
                    recorded_at: EvidenceValue::NotRecorded,
                    summary: EvidenceValue::NotRecorded,
                    evidence: EvidenceValue::NotRecorded,
                });
            }
        }
        view
    }

    /// Read exact v2 identities and owned content. This deliberately does not
    /// inspect the mutable Settings registry or authorize controls from state.
    pub fn from_execution(
        snapshot: &DurableTurnSnapshot,
        content: &ExecutionContentStore,
    ) -> Result<Self, ExecutionContentError> {
        let execution = content.project(snapshot)?;
        let contract = snapshot.contract();
        let graph = contract
            .graph()
            .ok_or(ExecutionContentError::Invalid("snapshot has no graph"))?;
        let mut nodes = vec![];
        let mut edges = vec![];
        for node in &graph.nodes {
            let definition = match content.resolve_activation_evidence(&node.definition.snapshot) {
                Ok(ActivationEvidenceContent::Definition {
                    definition_id,
                    revision,
                    profile,
                    configuration,
                }) if definition_id == &node.definition.definition_id => {
                    let fields = serde_json::from_str::<Value>(configuration).ok();
                    let fields = fields.as_ref().and_then(Value::as_object);
                    EvidenceValue::available(ControlPlaneDefinition {
                        name: EvidenceValue::from_option(
                            fields.and_then(|fields| text(fields, &["name"]).map(str::to_string)),
                        ),
                        role: EvidenceValue::from_option(
                            fields.and_then(|fields| text(fields, &["role"]).map(str::to_string)),
                        ),
                        provider: EvidenceValue::available(profile.provider.clone()),
                        model: EvidenceValue::available(profile.model.clone()),
                        // Full serialized configuration may contain credentials.
                        // Only the explicitly named display fields cross this API.
                        instructions: fields
                            .and_then(|fields| text(fields, &["system_prompt"]))
                            .map_or(EvidenceValue::NotRecorded, bounded_text),
                        tools: EvidenceValue::available(profile.tools.clone()),
                        configuration_revision: EvidenceValue::available(*revision),
                        snapshot: EvidenceValue::available(
                            node.definition.snapshot.as_str().into(),
                        ),
                    })
                }
                Ok(_) => return Err(ExecutionContentError::Conflict),
                Err(ExecutionContentError::Invalid(
                    "missing or wrong role activation evidence",
                )) => EvidenceValue::Missing {
                    reference: node.definition.snapshot.as_str().into(),
                },
                Err(error) => return Err(error),
            };
            let label = match &definition {
                EvidenceValue::Available {
                    value:
                        ControlPlaneDefinition {
                            name: EvidenceValue::Available { value },
                            ..
                        },
                } => value.clone(),
                _ => node.definition.definition_id.as_str().into(),
            };
            let mut activations = vec![];
            for entry in execution
                .activations
                .iter()
                .filter(|entry| entry.activation.activation.node_id == node.node_id)
            {
                let activation = &entry.activation;
                let output = match &entry.output {
                    ContentResolution::Available { content, .. } => bounded_text(&content.text),
                    ContentResolution::Missing { reference } => EvidenceValue::Missing {
                        reference: reference.as_str().into(),
                    },
                    ContentResolution::NotRecorded => EvidenceValue::NotRecorded,
                };
                let usage = match &entry.output {
                    ContentResolution::Available { content, .. } => {
                        EvidenceValue::available(json!(content.usage))
                    }
                    _ => EvidenceValue::Unknown {
                        reason: "No complete usage record is attached to the accepted output."
                            .into(),
                    },
                };
                let partial_outputs = entry
                    .partial_outputs
                    .iter()
                    .map(|output| partial_output(&output.text, None, None))
                    .chain(
                        entry
                            .reserved_outputs
                            .iter()
                            .filter(|reserved| {
                                activation.output.as_ref() != Some(&reserved.reference)
                            })
                            .map(|reserved| {
                                partial_output(
                                    &reserved.content.output.text,
                                    Some(reserved.reference.as_str()),
                                    Some(reserved.content.original_byte_len),
                                )
                            }),
                    )
                    .collect();
                let reason =
                    activation
                        .failure
                        .as_ref()
                        .map_or(EvidenceValue::NotRecorded, |reference| {
                            match entry
                                .reserved_outputs
                                .iter()
                                .find(|output| output.reference == *reference)
                            {
                                Some(output) if output.content.is_truncated() => {
                                    EvidenceValue::Truncated {
                                        value: text_prefix(&output.content.output.text).into(),
                                        original_byte_len: output.content.original_byte_len,
                                    }
                                }
                                Some(output) => bounded_text(&output.content.output.text),
                                // Resource preparation can fail before an output
                                // reservation exists. The driver retains that exact
                                // reason as Guidance and binds it in FailActivation.
                                None => match content.resolve_activation_evidence(reference) {
                                    Ok(ActivationEvidenceContent::Guidance { text }) => {
                                        bounded_text(text)
                                    }
                                    _ => EvidenceValue::Missing {
                                        reference: reference.as_str().into(),
                                    },
                                },
                            }
                        });
                let resolved_input = match content.validate_input(snapshot, &activation.input) {
                    Ok(resolved) => bounded_json(json!({
                        "guidance": resolved.guidance,
                        "attachments": resolved.attachments,
                        "repository": resolved.repository,
                        "parents": resolved.parents,
                        "revision_context": resolved.revision_context,
                        "grant": resolved.grant,
                        "budget": resolved.budget,
                    })),
                    Err(error) => EvidenceValue::Unavailable {
                        reason: format!("Retained input could not be resolved: {error}"),
                    },
                };
                let guidance = contract.guidance().iter().filter(|item| item.activation == activation.activation)
                    .map(|item| {
                        let text = match content.resolve_activation_evidence(&item.instruction) {
                            Ok(ActivationEvidenceContent::Guidance { text }) => bounded_text(text),
                            Ok(_) => EvidenceValue::Unavailable { reason: "Guidance reference has another evidence role.".into() },
                            Err(error) => EvidenceValue::Unavailable { reason: error.to_string() },
                        };
                        json!({"handoff": item, "instruction": text,
                            "delivery": "Consult the exact control command receipt; a handoff alone does not prove actor delivery."})
                    }).collect::<Vec<_>>();
                let input = EvidenceValue::available(json!({
                    "manifest": activation.input,
                    "resolved": resolved_input,
                    "safe_boundary_guidance": guidance,
                }));
                let mut evidence = vec![ControlPlaneEvidence {
                    kind: "acceptance".into(),
                    reference: EvidenceValue::from_option(
                        activation
                            .output
                            .as_ref()
                            .map(|reference| reference.as_str().into()),
                    ),
                    summary: EvidenceValue::available(
                        if entry.currently_accepted {
                            "Currently accepted generation"
                        } else {
                            "Not currently accepted"
                        }
                        .into(),
                    ),
                    recorded_at: EvidenceValue::NotRecorded,
                    details: EvidenceValue::available(
                        json!({ "checkpoint": activation.checkpoint, "currently_accepted": entry.currently_accepted }),
                    ),
                }];
                for blocker in contract
                    .blockers()
                    .iter()
                    .filter(|item| item.blocker.activation == activation.activation)
                {
                    let parameters =
                        match content.resolve_activation_evidence(&blocker.blocker.parameters) {
                            Ok(value) => bounded_json(json!(value)),
                            Err(error) => EvidenceValue::Unavailable {
                                reason: error.to_string(),
                            },
                        };
                    evidence.push(ControlPlaneEvidence {
                        kind: "typed_blocker".into(), reference: EvidenceValue::available(blocker.blocker.parameters.as_str().into()),
                        summary: EvidenceValue::available("Recorded typed wait; only an exact live host capability can resume it.".into()),
                        recorded_at: EvidenceValue::NotRecorded,
                        details: bounded_json(json!({"blocker":blocker,"parameters":parameters})),
                    });
                }
                for observation in &entry.stream {
                    use axocoatl_session::execution_content::ActivationStreamPayload;
                    let (kind, summary) = match &observation.content.payload {
                        ActivationStreamPayload::Text { delta } => {
                            ("stream_text", bounded_text(delta))
                        }
                        ActivationStreamPayload::ProviderRetry { reason } => {
                            ("provider_retry", bounded_text(reason))
                        }
                        ActivationStreamPayload::ReasoningSummary { delta } => {
                            ("reasoning_summary", bounded_text(delta))
                        }
                        ActivationStreamPayload::ToolProposed { name, .. } => {
                            ("tool_proposed", bounded_text(name))
                        }
                        ActivationStreamPayload::ToolResult { name, .. } => {
                            ("tool_observed_result", bounded_text(name))
                        }
                    };
                    evidence.push(ControlPlaneEvidence {
                        kind: kind.into(),
                        reference: EvidenceValue::available(observation.reference.as_str().into()),
                        summary,
                        recorded_at: EvidenceValue::available(
                            observation.content.recorded_at_unix_ms,
                        ),
                        details: bounded_json(json!(observation.content)),
                    });
                }
                activations.push(ControlPlaneActivation {
                    reference: ControlPlaneActivationRef::Exact {
                        activation: activation.activation.clone(),
                    },
                    generation: EvidenceValue::available(activation.activation.generation),
                    state: activation_state(activation.state).into(),
                    reason,
                    started_at: EvidenceValue::NotRecorded,
                    completed_at: EvidenceValue::NotRecorded,
                    input,
                    output,
                    partial_outputs,
                    usage,
                    capabilities: ControlPlaneCapabilities::read_only(
                        if execution.state.is_closed() {
                            "This logical turn is closed."
                        } else {
                            "Live execution authority has not been attached to this read snapshot."
                        },
                    ),
                    evidence,
                });
                for parent in &activation.input.parents {
                    edges.push(ControlPlaneEdge {
                        id: format!(
                            "input:{}:{}",
                            activation.activation.activation_id.as_str(),
                            parent.activation.activation_id.as_str()
                        ),
                        kind: "accepted_input".into(),
                        source: parent.activation.node_id.as_str().into(),
                        target: node.node_id.as_str().into(),
                        generation: EvidenceValue::available(activation.activation.generation),
                        recorded_at: EvidenceValue::NotRecorded,
                        summary: EvidenceValue::available(format!(
                            "Accepted generation {}",
                            parent.activation.generation
                        )),
                        evidence: EvidenceValue::available(parent.output.as_str().into()),
                    });
                }
            }
            nodes.push(ControlPlaneNode {
                node_id: node.node_id.as_str().into(),
                definition_id: node.definition.definition_id.as_str().into(),
                label,
                definition,
                dependencies: graph
                    .dependencies
                    .iter()
                    .filter(|edge| edge.child == node.node_id)
                    .map(|edge| edge.parent.as_str().into())
                    .collect(),
                activations,
            });
        }
        for edge in &graph.dependencies {
            edges.push(ControlPlaneEdge {
                id: format!(
                    "dependency:{}:{}:{}",
                    graph.snapshot_id.as_str(),
                    edge.parent.as_str(),
                    edge.child.as_str()
                ),
                kind: "dependency".into(),
                source: edge.parent.as_str().into(),
                target: edge.child.as_str().into(),
                generation: EvidenceValue::NotRecorded,
                recorded_at: EvidenceValue::NotRecorded,
                summary: EvidenceValue::NotRecorded,
                evidence: EvidenceValue::available(graph.snapshot_id.as_str().into()),
            });
        }
        Ok(Self {
            schema_version: 1,
            turn_controls: None,
            history_version: "execution_v2".into(),
            superseded_conversation: false,
            stop_requested: contract.stop_requested().cloned(),
            session_id: snapshot.owner().session_id.as_str().into(),
            turn_id: snapshot.turn_id().as_str().into(),
            state: logical_state(execution.state).into(),
            turn_revision: EvidenceValue::available(execution.revision),
            graph_revision: EvidenceValue::available(graph.revision),
            request: match &execution.request {
                ContentResolution::Available { content, .. } => bounded_text(&content.display_input),
                ContentResolution::Missing { reference } => EvidenceValue::Missing { reference: reference.as_str().into() },
                ContentResolution::NotRecorded => EvidenceValue::NotRecorded,
            },
            nodes,
            edges,
            epochs: EvidenceValue::available(execution.epochs.iter().map(|epoch| json!(epoch)).collect()),
            accepted_inputs: EvidenceValue::available(contract.current_accepted_activations().iter().map(|activation| json!({"activation": activation.activation, "input": activation.input, "checkpoint": activation.checkpoint, "output": activation.output})).collect()),
            invocations: EvidenceValue::available(contract.invocations().iter().map(|invocation| json!({"invocation_id": invocation.invocation_id, "activation": invocation.activation, "evidence": invocation.evidence, "disposition": invocation.evidence.disposition(), "scope": "canonical_turn_snapshot"})).collect()),
            conditions: EvidenceValue::available(graph.conditions.iter().map(|condition| json!({
                "definition": condition,
                "current_observation": contract.current_condition(&condition.condition_id),
                "observations": contract.conditions().iter().filter(|observation| observation.condition_id == condition.condition_id).collect::<Vec<_>>(),
                "runs": contract.condition_runs().iter().filter(|run| run.run.condition_id == condition.condition_id).collect::<Vec<_>>(),
            })).collect()),
            commands: EvidenceValue::Unavailable { reason: "The command receipt journal has not been joined to this snapshot.".into() },
            decisions: EvidenceValue::NotRecorded,
            warnings: vec!["Invocation evidence reflects this canonical turn snapshot. Later external-effect audit evidence has not been joined.".into()],
        })
    }
}

fn bounded_json(value: Value) -> EvidenceValue<Value> {
    let serialized = value.to_string();
    if serialized.len() <= TEXT_PREVIEW_BYTES {
        EvidenceValue::available(value)
    } else {
        EvidenceValue::Truncated {
            value: json!({"preview": text_prefix(&serialized)}),
            original_byte_len: serialized.len() as u64,
        }
    }
}

fn bounded_text(value: &str) -> EvidenceValue<String> {
    if value.len() <= TEXT_PREVIEW_BYTES {
        return EvidenceValue::available(value.into());
    }
    EvidenceValue::Truncated {
        value: text_prefix(value).into(),
        original_byte_len: value.len() as u64,
    }
}

fn text_prefix(value: &str) -> &str {
    let mut end = value.len().min(TEXT_PREVIEW_BYTES);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

fn partial_output(
    text: &str,
    reference: Option<&str>,
    original_byte_len: Option<u64>,
) -> ControlPlaneOutput {
    let original_byte_len = original_byte_len.unwrap_or(text.len() as u64);
    ControlPlaneOutput {
        text: text_prefix(text).into(),
        truncated: original_byte_len > text_prefix(text).len() as u64,
        original_byte_len: EvidenceValue::available(original_byte_len),
        reference: EvidenceValue::from_option(reference.map(str::to_string)),
    }
}

fn text<'a>(metadata: &'a Map<String, Value>, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|key| metadata.get(*key).and_then(Value::as_str))
}

fn text_value(metadata: &Map<String, Value>, keys: &[&str]) -> EvidenceValue<String> {
    text(metadata, keys).map_or(EvidenceValue::NotRecorded, bounded_text)
}

fn generation(metadata: &Map<String, Value>) -> Option<u32> {
    metadata
        .get("generation")
        .or_else(|| metadata.get("activation_generation"))
        .and_then(Value::as_u64)
        .and_then(|generation| u32::try_from(generation).ok())
}

fn string_list(value: &Value) -> Vec<String> {
    value
        .as_array()
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn selected_metadata(metadata: &Map<String, Value>, keys: &[&str]) -> Value {
    Value::Object(
        keys.iter()
            .filter_map(|key| {
                metadata
                    .get(*key)
                    .cloned()
                    .map(|value| ((*key).into(), value))
            })
            .collect(),
    )
}

fn legacy_node<'a>(nodes: &'a mut Vec<ControlPlaneNode>, id: &str) -> &'a mut ControlPlaneNode {
    let index = match nodes.iter().position(|node| node.node_id == id) {
        Some(index) => index,
        None => {
            nodes.push(ControlPlaneNode {
                node_id: id.into(),
                definition_id: id.into(),
                label: id.into(),
                definition: EvidenceValue::NotRecorded,
                dependencies: vec![],
                activations: vec![],
            });
            nodes.len() - 1
        }
    };
    &mut nodes[index]
}

fn legacy_activation<'a>(
    node: &'a mut ControlPlaneNode,
    turn: &SessionTurn,
    generation: Option<u32>,
) -> &'a mut ControlPlaneActivation {
    let index = match node.activations.iter().position(|activation| matches!(&activation.reference, ControlPlaneActivationRef::Legacy { generation: recorded, .. } if recorded == &generation)) {
        Some(index) => index,
        None => {
            node.activations.push(ControlPlaneActivation {
                reference: ControlPlaneActivationRef::Legacy { session_id: turn.session_id.clone(), turn_id: turn.id.clone(), node_id: node.node_id.clone(), generation },
                generation: EvidenceValue::from_option(generation),
                state: "unknown".into(), reason: EvidenceValue::NotRecorded,
                started_at: EvidenceValue::NotRecorded, completed_at: EvidenceValue::NotRecorded,
                input: EvidenceValue::NotRecorded, output: EvidenceValue::NotRecorded,
                partial_outputs: vec![], usage: EvidenceValue::Unknown { reason: "Usage was not recorded for this activation.".into() },
                capabilities: ControlPlaneCapabilities::read_only("Legacy history has no exact activation control identity."),
                evidence: vec![],
            });
            node.activations.len() - 1
        }
    };
    &mut node.activations[index]
}

fn legacy_event_state(kind: &str) -> Option<&'static str> {
    match kind {
        "coordination_agent_activated" => Some("running"),
        "coordination_agent_reactivated" => Some("waiting"),
        "coordination_agent_completed" => Some("completed"),
        "coordination_agent_failed" => Some("failed"),
        "coordination_agent_blocked" => Some("blocked"),
        "coordination_agent_cancelled" => Some("cancelled"),
        "agent_output_superseded" => Some("superseded"),
        _ => None,
    }
}

fn activation_state(state: ActivationState) -> &'static str {
    match state {
        ActivationState::Unstarted => "unstarted",
        ActivationState::Running => "running",
        ActivationState::Accepted => "accepted",
        ActivationState::Failed => "failed",
        ActivationState::Interrupted => "interrupted",
        ActivationState::Superseded => "superseded",
    }
}

fn logical_state(state: LogicalTurnState) -> &'static str {
    match state {
        LogicalTurnState::Running => "running",
        LogicalTurnState::NeedsAttention => "needs_attention",
        LogicalTurnState::Completed => "completed",
        LogicalTurnState::Cancelled => "cancelled",
        LogicalTurnState::Finished => "finished",
    }
}

#[cfg(test)]
#[path = "session_control_plane_tests.rs"]
mod tests;
