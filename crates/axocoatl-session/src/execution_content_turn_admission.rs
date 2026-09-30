//! Exact host input retained before first Begin; a separate handoff fact prevents
//! duplicate Send/reconnect from creating another execution driver after restart.
//! Neither record grants provider/tool authority or asserts unseen effects.
use super::*;
use crate::turn_contract::{
    CommandId, ExecutionEpochId, GrantSnapshotRef, TurnContractEvent, TurnGraphSnapshot, TurnNodeId,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TurnAdmissionNodeInput {
    pub node_id: TurnNodeId,
    /// Exactly one current request reference, followed by retained Guidance.
    pub guidance: Vec<EvidenceRef>,
    pub attachments: Vec<EvidenceRef>,
    pub budget: EvidenceRef,
    pub grant: GrantSnapshotRef,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TurnAdmissionContent {
    pub schema_version: u32,
    pub command_id: CommandId,
    pub turn_id: LogicalTurnId,
    pub epoch_id: ExecutionEpochId,
    /// Complete encoded host request, including team revision, grants, node
    /// evidence and exact user request. Never projected as Agent guidance.
    pub source: String,
    pub graph: TurnGraphSnapshot,
    pub request: EvidenceRef,
    pub nodes: Vec<TurnAdmissionNodeInput>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct DriverHandoff {
    admission: EvidenceRef,
}

impl ExecutionContentStore {
    pub fn turn_admission(
        &self,
        canonical: &SessionExecutionStore,
        turn: &LogicalTurnId,
    ) -> Result<Option<(&EvidenceRef, &TurnAdmissionContent)>, ExecutionContentError> {
        self.verify_provider_profile_owner(canonical)?;
        let selected = self
            .data
            .records
            .iter()
            .find_map(|record| match &record.body {
                Body::TurnAdmission(content) if &content.turn_id == turn => {
                    Some((&record.reference, content))
                }
                _ => None,
            });
        if let Some((_, content)) = selected {
            validate_canonical(canonical, content)?;
        }
        Ok(selected)
    }
    pub fn retain_turn_admission(
        &mut self,
        canonical: &SessionExecutionStore,
        content: TurnAdmissionContent,
    ) -> Result<DurableActivationEvidence, ExecutionContentError> {
        self.verify_provider_profile_owner(canonical)?;
        if let Some(record)=self.data.records.iter().find(|record|matches!(&record.body,
            Body::TurnAdmission(old) if old.turn_id==content.turn_id || old.command_id==content.command_id))
        {
            if record.body!=Body::TurnAdmission(content.clone()) { return Err(ExecutionContentError::Conflict); }
            validate_canonical(canonical,&content)?;
            return Ok(DurableActivationEvidence{identity:self.identity.clone(),reference:record.reference.clone()});
        }
        if canonical
            .turn(&content.turn_id)
            .map_err(canonical_error)?
            .is_some()
        {
            return Err(ExecutionContentError::Invalid(
                "first-turn source was not retained before Begin",
            ));
        }
        let reference = self.append(Body::TurnAdmission(content))?;
        Ok(DurableActivationEvidence {
            identity: self.identity.clone(),
            reference,
        })
    }
    pub fn turn_driver_handed_off(
        &self,
        canonical: &SessionExecutionStore,
        turn: &LogicalTurnId,
    ) -> Result<bool, ExecutionContentError> {
        let Some((admission, _)) = self.turn_admission(canonical, turn)? else {
            return Ok(false);
        };
        Ok(self.data.records.iter().any(|record| {
            matches!(&record.body,
            Body::DriverHandoff(handoff) if &handoff.admission==admission)
        }))
    }
    /// The host calls only after acquiring the real existing driver. This is
    /// immutable evidence of that transfer, never permission to replay it.
    pub fn retain_turn_driver_handoff(
        &mut self,
        canonical: &SessionExecutionStore,
        turn: &LogicalTurnId,
    ) -> Result<DurableActivationEvidence, ExecutionContentError> {
        let (admission, content) =
            self.turn_admission(canonical, turn)?
                .ok_or(ExecutionContentError::Invalid(
                    "driver lacks retained first-turn source",
                ))?;
        if canonical.turn(turn).map_err(canonical_error)?.is_none() {
            return Err(ExecutionContentError::Invalid(
                "driver handoff requires exact Begin",
            ));
        }
        validate_canonical(canonical, content)?;
        let body = Body::DriverHandoff(DriverHandoff {
            admission: admission.clone(),
        });
        let reference = self.append(body)?;
        Ok(DurableActivationEvidence {
            identity: self.identity.clone(),
            reference,
        })
    }
}
fn canonical_error(error: crate::execution_store::ExecutionStoreError) -> ExecutionContentError {
    ExecutionContentError::Io(io::Error::other(error))
}
fn validate_canonical(
    canonical: &SessionExecutionStore,
    content: &TurnAdmissionContent,
) -> Result<(), ExecutionContentError> {
    let records = canonical.records().map_err(canonical_error)?;
    if let Some(begin) = records
        .iter()
        .find(|record| record.turn_id == content.turn_id)
    {
        if begin.command_id != content.command_id
            || !matches!(&begin.event,
            TurnContractEvent::Begin{epoch_id,graph,..} if epoch_id==&content.epoch_id && graph==&content.graph)
            || canonical
                .snapshot(&content.turn_id)
                .map_err(canonical_error)?
                .request_ref()
                != Some(&content.request)
        {
            return Err(ExecutionContentError::Invalid(
                "retained first-turn source differs from exact Begin",
            ));
        }
    }
    Ok(())
}

pub(super) fn validate_admission(
    content: &TurnAdmissionContent,
    owner: &ExecutionStoreOwner,
) -> Result<(), ExecutionContentError> {
    if content.schema_version != 1 || content.source.is_empty() {
        return Err(ExecutionContentError::Invalid(
            "invalid first-turn admission source",
        ));
    }
    encode_bounded(content, MAX_TEXT)?;
    let source: serde_json::Value = serde_json::from_str(&content.source)?;
    if !source.is_object() {
        return Err(ExecutionContentError::Invalid(
            "first-turn source must be an object",
        ));
    }
    content
        .graph
        .validate(&owner.session_id)
        .map_err(|_| ExecutionContentError::Invalid("invalid first-turn admission graph"))?;
    if content.nodes.len() != content.graph.nodes.len() {
        return Err(ExecutionContentError::Invalid(
            "first-turn inputs do not cover the graph",
        ));
    }
    let mut nodes = HashSet::new();
    for node in &content.nodes {
        if !nodes.insert(&node.node_id)
            || !content
                .graph
                .nodes
                .iter()
                .any(|graph| graph.node_id == node.node_id)
            || node.guidance.first() != Some(&content.request)
            || node
                .guidance
                .iter()
                .filter(|reference| *reference == &content.request)
                .count()
                != 1
            || node.guidance.len().saturating_add(node.attachments.len())
                > crate::turn_contract::MAX_INPUT_REFERENCES
        {
            return Err(ExecutionContentError::Invalid(
                "invalid first-turn node evidence",
            ));
        }
    }
    Ok(())
}
pub(super) fn validate_admission_next(
    records: &[Record],
    content: &TurnAdmissionContent,
) -> Result<(), ExecutionContentError> {
    if records.iter().any(|record|matches!(&record.body,Body::TurnAdmission(old) if old.turn_id==content.turn_id || old.command_id==content.command_id)) {
        return Err(ExecutionContentError::Conflict);
    }
    let body = |reference: &EvidenceRef| {
        records
            .iter()
            .find(|record| &record.reference == reference)
            .map(|record| &record.body)
    };
    if !matches!(body(&content.request),Some(Body::Request(request)) if request.turn_id==content.turn_id)
    {
        return Err(ExecutionContentError::Invalid(
            "first-turn admission lacks exact request",
        ));
    }
    for node in &content.nodes {
        let graph = content
            .graph
            .nodes
            .iter()
            .find(|graph| graph.node_id == node.node_id)
            .unwrap();
        let Some(Body::ActivationEvidence(ActivationEvidenceContent::Definition {
            definition_id,
            profile,
            ..
        })) = body(&graph.definition.snapshot)
        else {
            return Err(ExecutionContentError::Invalid(
                "first-turn definition is unavailable",
            ));
        };
        let Some(Body::ActivationEvidence(ActivationEvidenceContent::Budget { limits })) =
            body(&node.budget)
        else {
            return Err(ExecutionContentError::Invalid(
                "first-turn budget is unavailable",
            ));
        };
        let Some(Body::ActivationEvidence(ActivationEvidenceContent::Grant { policy })) =
            body(&node.grant.evidence)
        else {
            return Err(ExecutionContentError::Invalid(
                "first-turn grant is unavailable",
            ));
        };
        if definition_id != &graph.definition.definition_id
            || policy.id != node.grant.grant_id.as_str()
            || policy.revision != node.grant.revision
            || policy.holder != node.node_id
            || &policy.limits != limits
            || !policy.profiles.contains(profile)
        {
            return Err(ExecutionContentError::Invalid(
                "first-turn grant, budget or definition binding differs",
            ));
        }
        for reference in node.guidance.iter().skip(1) {
            if !matches!(
                body(reference),
                Some(Body::ActivationEvidence(
                    ActivationEvidenceContent::Guidance { .. }
                ))
            ) {
                return Err(ExecutionContentError::Invalid(
                    "additional first-turn guidance must be retained Guidance",
                ));
            }
        }
        for reference in &node.attachments {
            if !matches!(
                body(reference),
                Some(Body::ActivationEvidence(
                    ActivationEvidenceContent::Attachment { .. }
                        | ActivationEvidenceContent::BinaryAttachment { .. }
                ))
            ) {
                return Err(ExecutionContentError::Invalid(
                    "first-turn attachment must be retained Attachment",
                ));
            }
        }
    }
    Ok(())
}
pub(super) fn validate_handoff_next(
    records: &[Record],
    handoff: &DriverHandoff,
) -> Result<(), ExecutionContentError> {
    if !records.iter().any(|record| {
        record.reference == handoff.admission && matches!(&record.body, Body::TurnAdmission(_))
    }) {
        return Err(ExecutionContentError::Invalid(
            "driver handoff lacks exact retained admission",
        ));
    }
    if records.iter().any(|record|matches!(&record.body,Body::DriverHandoff(old) if old.admission==handoff.admission)) {return Err(ExecutionContentError::Conflict);}
    Ok(())
}

/// Transfer of the existing driver after one exact canonical human control.
/// A different control must establish a new canonical transition before another
/// transfer; repeating a receipt is never permission to replay execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ControlDriverHandoff {
    admission: EvidenceRef,
    command_id: CommandId,
    canonical_command_id: CommandId,
}
impl ExecutionContentStore {
    pub fn control_driver_handed_off(
        &self,
        canonical: &SessionExecutionStore,
        turn: &LogicalTurnId,
        command: &CommandId,
    ) -> Result<bool, ExecutionContentError> {
        let Some((admission, _)) = self.turn_admission(canonical, turn)? else {
            return Ok(false);
        };
        Ok(self.data.records.iter().any(|record| {
            matches!(&record.body,Body::ControlDriverHandoff(handoff)
            if &handoff.admission==admission && &handoff.command_id==command)
        }))
    }
    pub fn retain_control_driver_handoff(
        &mut self,
        canonical: &SessionExecutionStore,
        turn: &LogicalTurnId,
        command: CommandId,
        canonical_command: CommandId,
    ) -> Result<DurableActivationEvidence, ExecutionContentError> {
        let (admission, _) =
            self.turn_admission(canonical, turn)?
                .ok_or(ExecutionContentError::Invalid(
                    "control driver lacks native admission",
                ))?;
        if !canonical
            .records()
            .map_err(canonical_error)?
            .iter()
            .any(|event| {
                &event.turn_id == turn
                    && event.command_id == canonical_command
                    && matches!(
                        event.event,
                        TurnContractEvent::Continue { .. }
                            | TurnContractEvent::StartActivation { .. }
                            | TurnContractEvent::ReviseAccepted { .. }
                    )
            })
        {
            return Err(ExecutionContentError::Invalid(
                "control driver lacks exact canonical transition",
            ));
        }
        let reference = self.append(Body::ControlDriverHandoff(ControlDriverHandoff {
            admission: admission.clone(),
            command_id: command,
            canonical_command_id: canonical_command,
        }))?;
        Ok(DurableActivationEvidence {
            identity: self.identity.clone(),
            reference,
        })
    }
}
pub(super) fn validate_control_handoff_next(
    records: &[Record],
    handoff: &ControlDriverHandoff,
) -> Result<(), ExecutionContentError> {
    if !records.iter().any(|record| {
        record.reference == handoff.admission && matches!(record.body, Body::TurnAdmission(_))
    }) {
        return Err(ExecutionContentError::Invalid(
            "control driver handoff lacks exact admission",
        ));
    }
    if records.iter().any(|record| {
        matches!(&record.body,Body::ControlDriverHandoff(old)
        if old.admission==handoff.admission && old.command_id==handoff.command_id)
    }) {
        return Err(ExecutionContentError::Conflict);
    }
    Ok(())
}
