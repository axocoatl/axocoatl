//! Bounded, immutable actor observations in the existing content journal.
//! These records do not admit tools, settle effects, or accept model state.
use super::*;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ActivationStreamPayload {
    Text {
        delta: String,
    },
    /// The provider response ended before its completion record and the
    /// actor started one retry. Text observed before this marker belongs to
    /// the abandoned attempt.
    ProviderRetry {
        reason: String,
    },
    ReasoningSummary {
        delta: String,
    },
    /// The actor surfaced a proposed call after policy hooks. Actual execution
    /// still requires the independent canonical/audit dispatch boundary.
    ToolProposed {
        call_id: String,
        name: String,
        provider_response_group: u64,
        provider_call_index: usize,
        provider_call_count: usize,
        arguments_sha256: String,
        arguments_bytes: u64,
    },
    /// Actor-visible result after policy hooks, not the raw backend outcome.
    /// Protected actual outcomes remain in the independent invocation audit.
    ToolResult {
        call_id: String,
        name: String,
        result_sha256: String,
        result_bytes: u64,
        is_error: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActivationStreamContent {
    pub schema_version: u32,
    pub activation: ActivationRef,
    pub sequence: u64,
    pub recorded_at_unix_ms: u64,
    pub payload: ActivationStreamPayload,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ActivationStreamView {
    pub reference: EvidenceRef,
    pub content: ActivationStreamContent,
}

impl ExecutionContentStore {
    /// The host must hold the exact live generation gate while recording.
    /// The store proves retained canonical identity, contiguous event ordering,
    /// existing content bounds, and preservation of every settlement reservation.
    /// No unbounded queue or additional storage/default budget is introduced.
    pub fn record_activation_stream(
        &mut self,
        snapshot: &DurableTurnSnapshot,
        content: ActivationStreamContent,
    ) -> Result<ActivationStreamView, ExecutionContentError> {
        // A prepared/unknown actor cannot publish speculative activity. Exact
        // repeats of already retained evidence remain readable through stream().
        let candidate = self.require_activation(snapshot, &content.activation)?;
        if candidate.state != ActivationState::Running
            || snapshot.contract().state() != Some(LogicalTurnState::Running)
        {
            return Err(ExecutionContentError::Invalid(
                "stream producer is not running",
            ));
        }
        let reference = self.append(Body::ActivationStream(content.clone()))?;
        Ok(ActivationStreamView { reference, content })
    }

    pub fn activation_stream(
        &self,
        snapshot: &DurableTurnSnapshot,
        activation: &ActivationRef,
    ) -> Result<Vec<ActivationStreamView>, ExecutionContentError> {
        self.require_activation(snapshot, activation)?;
        Ok(self
            .data
            .records
            .iter()
            .filter_map(|record| match &record.body {
                Body::ActivationStream(content) if &content.activation == activation => {
                    Some(ActivationStreamView {
                        reference: record.reference.clone(),
                        content: content.clone(),
                    })
                }
                _ => None,
            })
            .collect())
    }
}

pub(super) fn validate_stream(
    content: &ActivationStreamContent,
    owner: &ExecutionStoreOwner,
) -> Result<(), ExecutionContentError> {
    if content.schema_version != 1 || content.activation.generation == 0 {
        return Err(ExecutionContentError::Invalid(
            "unsupported or invalid activation stream",
        ));
    }
    if content.activation.session_id != owner.session_id {
        return Err(ExecutionContentError::OwnerMismatch);
    }
    match &content.payload {
        ActivationStreamPayload::ToolProposed {
            provider_call_index,
            provider_call_count,
            arguments_sha256,
            arguments_bytes,
            ..
        } => {
            if *provider_call_count == 0
                || provider_call_index >= provider_call_count
                || !valid_digest(arguments_sha256)
                || *arguments_bytes > MAX_TOOL_BYTES as u64
            {
                return Err(ExecutionContentError::Invalid(
                    "invalid proposed tool observation",
                ));
            }
        }
        ActivationStreamPayload::ToolResult {
            result_sha256,
            result_bytes,
            ..
        } => {
            if !valid_digest(result_sha256) || *result_bytes > MAX_TOOL_BYTES as u64 {
                return Err(ExecutionContentError::Invalid(
                    "invalid actor result observation",
                ));
            }
        }
        ActivationStreamPayload::ProviderRetry { reason } => {
            if reason.is_empty() || reason.len() > 1024 {
                return Err(ExecutionContentError::Invalid(
                    "invalid provider retry observation",
                ));
            }
        }
        ActivationStreamPayload::Text { .. } | ActivationStreamPayload::ReasoningSummary { .. } => {
        }
    }
    encode_bounded(content, MAX_TOOL_BYTES)?;
    Ok(())
}

pub(super) fn validate_stream_next(
    records: &[Record],
    content: &ActivationStreamContent,
) -> Result<(), ExecutionContentError> {
    let preceding = records
        .iter()
        .filter(|record| {
            matches!(&record.body,
        Body::ActivationStream(old) if old.activation == content.activation)
        })
        .count();
    if content.sequence != preceding as u64 {
        return Err(ExecutionContentError::Conflict);
    }
    // No new live observation can be appended after the actor's terminal body.
    // Recovery reads old events without constructing a new writer or producer.
    if records.iter().any(|record| match &record.body {
        Body::Output(output) => {
            output.activation == content.activation && output.kind == OutputKind::Final
        }
        Body::ReservedOutput(output) => {
            output.output.activation == content.activation
                && output.slot == ActivationOutputSlot::Settlement
        }
        _ => false,
    }) {
        return Err(ExecutionContentError::Conflict);
    }
    Ok(())
}
