//! Durable observation of explicit physical ownership reacquisition. This is
//! historical evidence, never permission to recreate an owner or replay work.
use super::*;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoryReattachment {
    pub turn_id: LogicalTurnId,
    pub original: EvidenceRef,
    pub acquired: EvidenceRef,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RepositoryReattachmentView {
    pub reference: EvidenceRef,
    pub content: RepositoryReattachment,
    pub original_resource: ActivationEvidenceContent,
    pub acquired_resource: ActivationEvidenceContent,
}

impl ExecutionContentStore {
    pub fn retain_repository_reattachment(
        &mut self,
        snapshot: &DurableTurnSnapshot,
        proof: RepositoryReattachment,
    ) -> Result<EvidenceRef, ExecutionContentError> {
        self.require_snapshot(snapshot)?;
        if &proof.turn_id != snapshot.turn_id() || !snapshot.contract().activations().iter().any(|item|
            matches!(&item.input.repository, RepositoryInput::Recorded { snapshot } if snapshot == &proof.original)) {
            return Err(ExecutionContentError::Invalid("reattachment must identify this turn's retained repository input"));
        }
        self.append(Body::RepositoryReattachment(proof))
    }

    pub fn repository_reattachment(
        &self,
        reference: &EvidenceRef,
    ) -> Result<RepositoryReattachment, ExecutionContentError> {
        self.healthy()?;
        match self.record(reference)?.as_ref().map(|record| &record.body) {
            Some(Body::RepositoryReattachment(proof)) => Ok(proof.clone()),
            _ => Err(ExecutionContentError::Invalid(
                "missing repository reattachment proof",
            )),
        }
    }

    pub fn repository_reattachments(
        &self,
        snapshot: &DurableTurnSnapshot,
    ) -> Result<Vec<RepositoryReattachmentView>, ExecutionContentError> {
        self.require_snapshot(snapshot)?;
        self.keyed(&segments::reattachment_key(snapshot.turn_id()))?
            .iter()
            .filter_map(|record| match &record.body {
                Body::RepositoryReattachment(proof) if &proof.turn_id == snapshot.turn_id() => {
                    Some((record, proof))
                }
                _ => None,
            })
            .map(|(record, proof)| {
                Ok(RepositoryReattachmentView {
                    reference: record.reference.clone(),
                    content: proof.clone(),
                    original_resource: self.resolve_activation_evidence(&proof.original)?.clone(),
                    acquired_resource: self.resolve_activation_evidence(&proof.acquired)?.clone(),
                })
            })
            .collect()
    }
}
