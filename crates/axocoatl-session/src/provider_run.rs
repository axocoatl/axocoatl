//! Shared, retained provider-run identity and capability observations.
//!
//! These records describe evidence, not operation authority. Requested settings
//! never establish observed provider/model identity. A host must resolve the
//! evidence, bind any protocol request/response, and enforce its own grant,
//! effect and live-owner checks before using a record for execution.
use crate::turn_contract::{ActivationRef, EvidenceRef, MAX_CONTRACT_ENVELOPE_BYTES};
use serde::{Deserialize, Serialize};
use std::io::{self, Write};

pub const PROVIDER_RUN_SCHEMA_VERSION: u32 = 1;
// Existing external_harness::validate_id representation bound, shared unchanged.
pub const MAX_PROVIDER_IDENTITY_BYTES: usize = 256;

#[derive(Debug, thiserror::Error)]
pub enum ProviderRunError {
    #[error("unsupported provider-run or capability schema")]
    Version,
    #[error("provider-run identity or evidence relationship is invalid")]
    Identity,
    #[error("provider-run representation exceeds its existing bound")]
    Capacity,
    #[error("provider-run JSON: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum RunFact<T> {
    Known {
        value: T,
    },
    /// Not established by retained evidence. This is not an empty identifier,
    /// a default provider/model, nor proof that a remote identity does not exist.
    Unknown,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderIdentity {
    pub provider: RunFact<String>,
    pub model: RunFact<String>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutorKind {
    Native,
    ExternalHarness,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VersionedExecutorComponent {
    pub name: String,
    pub version: RunFact<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutorIdentity {
    pub kind: ExecutorKind,
    pub adapter: VersionedExecutorComponent,
    pub protocol: RunFact<VersionedExecutorComponent>,
}
/// Preserve a protocol's string/integer identity distinction without coercion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ExternalIdentity {
    Text(String),
    Integer(i64),
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalRunIdentity {
    pub session_id: RunFact<ExternalIdentity>,
    pub run_id: RunFact<ExternalIdentity>,
    /// A remote request identity in the named protocol, when actually recorded.
    /// A client JSON-RPC correlation number does not manufacture this fact.
    pub request_id: RunFact<ExternalIdentity>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderRunRef {
    pub schema_version: u32,
    pub activation: ActivationRef,
    /// Existing local call/request identity, independent of remote identifiers.
    pub local_request_id: String,
    pub executor: ExecutorIdentity,
    pub requested: ProviderIdentity,
    pub observed: ProviderIdentity,
    pub external: ExternalRunIdentity,
    /// Exact retained request/observation provenance; a reference is not proof.
    pub evidence: EvidenceRef,
}
impl ProviderRunRef {
    pub fn decode(bytes: &[u8]) -> Result<Self, ProviderRunError> {
        if bytes.len() > MAX_CONTRACT_ENVELOPE_BYTES {
            return Err(ProviderRunError::Capacity);
        }
        #[derive(Deserialize)]
        struct Header {
            schema_version: u32,
        }
        if serde_json::from_slice::<Header>(bytes)?.schema_version != PROVIDER_RUN_SCHEMA_VERSION {
            return Err(ProviderRunError::Version);
        }
        let value: Self = serde_json::from_slice(bytes)?;
        value.validate()?;
        Ok(value)
    }
    pub fn validate_for(&self, activation: &ActivationRef) -> Result<(), ProviderRunError> {
        self.validate()?;
        if &self.activation != activation {
            return Err(ProviderRunError::Identity);
        }
        Ok(())
    }
    pub fn validate(&self) -> Result<(), ProviderRunError> {
        if self.schema_version != PROVIDER_RUN_SCHEMA_VERSION {
            return Err(ProviderRunError::Version);
        }
        if self.activation.generation == 0 {
            return Err(ProviderRunError::Identity);
        }
        identity(&self.local_request_id)?;
        component(&self.executor.adapter)?;
        if let RunFact::Known { value } = &self.executor.protocol {
            component(value)?;
        }
        for value in [
            &self.requested.provider,
            &self.requested.model,
            &self.observed.provider,
            &self.observed.model,
        ] {
            if let RunFact::Known { value } = value {
                identity(value)?;
            }
        }
        for value in [
            &self.external.session_id,
            &self.external.run_id,
            &self.external.request_id,
        ] {
            if let RunFact::Known {
                value: ExternalIdentity::Text(value),
            } = value
            {
                identity(value)?;
            }
        }
        bounded_serialization(self)
    }
}

/// Existing harness observation states retain their exact wire representation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityEvidence {
    Observed,
    SchemaOnly,
    /// Evidence is absent in the observed configuration; no support is inferred.
    Unavailable,
    /// Explicit evidence establishes that this operation is unsupported.
    Unsupported,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SteeringCapabilities {
    pub native: CapabilityEvidence,
    pub next_safe_boundary: CapabilityEvidence,
    pub interrupt_and_revise: CapabilityEvidence,
}
/// Shared executor envelope, moved from the pinned harness without changing its
/// existing fields. New optional dimensions preserve legacy bytes; absence is
/// projected as Unavailable rather than synthesizing a native capability.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutorCapabilities {
    pub schema_version: u32,
    pub start: CapabilityEvidence,
    pub streamed_items: CapabilityEvidence,
    pub usage: CapabilityEvidence,
    pub whole_run_stop: CapabilityEvidence,
    pub native_steer: CapabilityEvidence,
    pub retained_history: CapabilityEvidence,
    pub child_stop: CapabilityEvidence,
    pub exact_retry: CapabilityEvidence,
    pub cross_process_resume: CapabilityEvidence,
    pub side_effect_rollback: CapabilityEvidence,
    pub enforced_budget: CapabilityEvidence,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_safe_boundary_steer: Option<CapabilityEvidence>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interrupt_and_revise: Option<CapabilityEvidence>,
}
impl ExecutorCapabilities {
    pub fn decode(bytes: &[u8]) -> Result<Self, ProviderRunError> {
        if bytes.len() > MAX_CONTRACT_ENVELOPE_BYTES {
            return Err(ProviderRunError::Capacity);
        }
        #[derive(Deserialize)]
        struct Header {
            schema_version: u32,
        }
        if serde_json::from_slice::<Header>(bytes)?.schema_version != 1 {
            return Err(ProviderRunError::Version);
        }
        let value: Self = serde_json::from_slice(bytes)?;
        value.validate()?;
        Ok(value)
    }
    pub fn validate(&self) -> Result<(), ProviderRunError> {
        if self.schema_version != 1 {
            return Err(ProviderRunError::Version);
        }
        bounded_serialization(self)
    }
    pub fn steering(&self) -> Result<SteeringCapabilities, ProviderRunError> {
        self.validate()?;
        Ok(SteeringCapabilities {
            native: self.native_steer,
            next_safe_boundary: self
                .next_safe_boundary_steer
                .unwrap_or(CapabilityEvidence::Unavailable),
            interrupt_and_revise: self
                .interrupt_and_revise
                .unwrap_or(CapabilityEvidence::Unavailable),
        })
    }
}

fn identity(value: &str) -> Result<(), ProviderRunError> {
    if value.is_empty()
        || value.len() > MAX_PROVIDER_IDENTITY_BYTES
        || value.chars().any(char::is_control)
    {
        return Err(ProviderRunError::Identity);
    }
    Ok(())
}
fn component(value: &VersionedExecutorComponent) -> Result<(), ProviderRunError> {
    identity(&value.name)?;
    if let RunFact::Known { value } = &value.version {
        identity(value)?;
    }
    Ok(())
}
fn bounded_serialization(value: &impl Serialize) -> Result<(), ProviderRunError> {
    struct Counter(usize);
    impl Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0 = self
                .0
                .checked_add(bytes.len())
                .filter(|n| *n <= MAX_CONTRACT_ENVELOPE_BYTES)
                .ok_or_else(|| io::Error::other("provider run envelope limit"))?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    serde_json::to_writer(Counter(0), value).map_err(|_| ProviderRunError::Capacity)
}
