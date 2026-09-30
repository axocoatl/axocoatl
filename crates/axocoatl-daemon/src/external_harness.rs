//! Capability-limited mapping for the pinned external executor acceptance fixture.
//!
//! This module does not dispatch a provider request or authorize a tool. A host must
//! durably record its invocation before sending anything, retain the exact external
//! run identity, and route server requests through its authority boundary. Protocol
//! observations never establish rollback, native child control, or safe replay.

use axocoatl_session::turn_contract::ActivationRef;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

pub const CODEX_FIXTURE_VERSION: &str = "0.153.4";
pub const CODEX_FIXTURE_BINARY_SHA256: &str =
    "a30ec314bbd0e3721632234d07db7c99855db3b9f1e32dbe8c791947f07e7629";
pub const CODEX_FIXTURE_SCHEMA_SHA256: &str =
    "03cd0961387d55845ca2ac1cb7127a9a9724d31ec53897b5a2993b4541168a7b";
/// Separately dated official release pin; the original app-bundle fixture stays
/// immutable. Both exports have the exact same protocol-schema digest.
pub const CODEX_OFFICIAL_FIXTURE_BINARY_SHA256: &str =
    "b973d440acac501fd2594a43e7ca9ce41e0a65b9dfb28d0d7a7837c99e1261e3";
pub const MAX_HARNESS_FRAME_BYTES: usize = 256 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum HarnessProtocolError {
    #[error("external harness executable or protocol differs from the accepted pin")]
    PinMismatch,
    #[error("external harness frame exceeds its evidence bound")]
    FrameTooLarge,
    #[error("invalid external harness frame: {0}")]
    Json(#[from] serde_json::Error),
    #[error("external harness frame has no valid {0}")]
    MissingField(&'static str),
    #[error("external harness event belongs to another activation")]
    StaleActivation,
    #[error("shared provider-run reference: {0}")]
    RunReference(#[from] axocoatl_session::provider_run::ProviderRunError),
}

/// The pin is checked against the actual executable, not only its version output.
pub fn verify_codex_fixture_pin(
    version: &str,
    executable: &[u8],
    schema_sha256: &str,
) -> Result<(), HarnessProtocolError> {
    if version != CODEX_FIXTURE_VERSION
        || hex::encode(Sha256::digest(executable)) != CODEX_FIXTURE_BINARY_SHA256
        || schema_sha256 != CODEX_FIXTURE_SCHEMA_SHA256
    {
        return Err(HarnessProtocolError::PinMismatch);
    }
    Ok(())
}

/// Verify the official release artifact independently of the original bundle.
pub fn verify_codex_official_fixture_pin(
    version: &str,
    executable: &[u8],
    schema_sha256: &str,
) -> Result<(), HarnessProtocolError> {
    if version != CODEX_FIXTURE_VERSION
        || hex::encode(Sha256::digest(executable)) != CODEX_OFFICIAL_FIXTURE_BINARY_SHA256
        || schema_sha256 != CODEX_FIXTURE_SCHEMA_SHA256
    {
        return Err(HarnessProtocolError::PinMismatch);
    }
    Ok(())
}

// Preserve existing import paths while sharing the serialized envelope.
pub use axocoatl_session::provider_run::{
    CapabilityEvidence, ExecutorCapabilities as HarnessCapabilities,
};

pub fn codex_fixture_capabilities() -> HarnessCapabilities {
    use CapabilityEvidence::{Observed, SchemaOnly, Unavailable};
    HarnessCapabilities {
        schema_version: 1,
        start: Observed,
        streamed_items: Observed,
        usage: Observed,
        whole_run_stop: Observed,
        // Only stale-precondition rejection was observed, not accepted steering.
        native_steer: SchemaOnly,
        retained_history: Unavailable,
        child_stop: Unavailable,
        exact_retry: Unavailable,
        cross_process_resume: Unavailable,
        side_effect_rollback: Unavailable,
        enforced_budget: Unavailable,
        // No additional steering behavior was observed by the pinned fixture.
        next_safe_boundary_steer: None,
        interrupt_and_revise: None,
    }
}

/// Observed on the separately pinned official release with a persisted thread.
/// A read after process loss is not continuation or proof of effect settlement.
pub fn codex_official_fixture_capabilities() -> HarnessCapabilities {
    let mut capabilities = codex_fixture_capabilities();
    capabilities.retained_history = CapabilityEvidence::Observed;
    capabilities
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderRunRef {
    pub schema_version: u32,
    pub activation: ActivationRef,
    pub adapter_version: String,
    pub executable_sha256: String,
    pub schema_sha256: String,
    pub provider_thread_id: String,
    pub provider_turn_id: String,
}

impl ProviderRunRef {
    pub fn codex_fixture(
        activation: ActivationRef,
        provider_thread_id: String,
        provider_turn_id: String,
    ) -> Result<Self, HarnessProtocolError> {
        validate_id(&provider_thread_id)?;
        validate_id(&provider_turn_id)?;
        Ok(Self {
            schema_version: 1,
            activation,
            adapter_version: CODEX_FIXTURE_VERSION.to_owned(),
            executable_sha256: CODEX_FIXTURE_BINARY_SHA256.to_owned(),
            schema_sha256: CODEX_FIXTURE_SCHEMA_SHA256.to_owned(),
            provider_thread_id,
            provider_turn_id,
        })
    }

    /// The official release is a distinct executable pin, even with equal version
    /// and schema. The host verifies that executable before using this identity.
    pub fn codex_official_fixture(
        activation: ActivationRef,
        provider_thread_id: String,
        provider_turn_id: String,
    ) -> Result<Self, HarnessProtocolError> {
        let mut reference = Self::codex_fixture(activation, provider_thread_id, provider_turn_id)?;
        reference.executable_sha256 = CODEX_OFFICIAL_FIXTURE_BINARY_SHA256.to_owned();
        Ok(reference)
    }

    /// The owned transport disappearing never establishes a terminal outcome.
    /// The host retains this observation against this exact activation/run and
    /// must not automatically replay it or fill missing usage with zero.
    pub fn transport_lost(
        &self,
        current_activation: &ActivationRef,
    ) -> Result<HarnessObservation, HarnessProtocolError> {
        self.decode(current_activation, b"{}")?;
        Ok(HarnessObservation::TransportLostOutcomeUnknown)
    }

    /// Project the existing pinned run into the shared identity contract. All
    /// request/observation facts come from retained host evidence, not a model
    /// name inferred from the adapter. Legacy fixture serialization is unchanged.
    pub fn provider_run_reference(
        &self,
        local_request_id: String,
        requested: axocoatl_session::provider_run::ProviderIdentity,
        observed: axocoatl_session::provider_run::ProviderIdentity,
        remote_request_id: axocoatl_session::provider_run::RunFact<
            axocoatl_session::provider_run::ExternalIdentity,
        >,
        evidence: axocoatl_session::turn_contract::EvidenceRef,
    ) -> Result<axocoatl_session::provider_run::ProviderRunRef, HarnessProtocolError> {
        use axocoatl_session::provider_run::{
            ExecutorIdentity, ExecutorKind, ExternalIdentity, ExternalRunIdentity,
            ProviderRunRef as SharedRunRef, RunFact, VersionedExecutorComponent,
        };
        // Reuse exact existing pin/identity/activation validation without dispatch.
        self.decode(&self.activation, b"{}")?;
        let value = SharedRunRef {
            schema_version: 1,
            activation: self.activation.clone(),
            local_request_id,
            executor: ExecutorIdentity {
                kind: ExecutorKind::ExternalHarness,
                adapter: VersionedExecutorComponent {
                    name: "codex-app-server".into(),
                    version: RunFact::Known {
                        value: self.adapter_version.clone(),
                    },
                },
                protocol: RunFact::Known {
                    value: VersionedExecutorComponent {
                        name: "codex-app-server-jsonrpc".into(),
                        version: RunFact::Known { value: "v2".into() },
                    },
                },
            },
            requested,
            observed,
            external: ExternalRunIdentity {
                session_id: RunFact::Known {
                    value: ExternalIdentity::Text(self.provider_thread_id.clone()),
                },
                run_id: RunFact::Known {
                    value: ExternalIdentity::Text(self.provider_turn_id.clone()),
                },
                request_id: remote_request_id,
            },
            evidence,
        };
        value
            .validate()
            .map_err(HarnessProtocolError::RunReference)?;
        Ok(value)
    }

    /// Whole external run cancellation is never labelled as internal child Stop.
    pub fn interrupt_request(&self, request_id: u64) -> Value {
        json!({"id": request_id, "method": "turn/interrupt", "params": {
            "threadId": self.provider_thread_id, "turnId": self.provider_turn_id
        }})
    }

    /// Decode only evidence for this exact activation and external run.
    /// Server requests are returned to the authority caller and never approved here.
    pub fn decode(
        &self,
        current_activation: &ActivationRef,
        frame: &[u8],
    ) -> Result<HarnessObservation, HarnessProtocolError> {
        if self.schema_version != 1
            || self.adapter_version != CODEX_FIXTURE_VERSION
            || (self.executable_sha256 != CODEX_FIXTURE_BINARY_SHA256
                && self.executable_sha256 != CODEX_OFFICIAL_FIXTURE_BINARY_SHA256)
            || self.schema_sha256 != CODEX_FIXTURE_SCHEMA_SHA256
        {
            return Err(HarnessProtocolError::PinMismatch);
        }
        validate_id(&self.provider_thread_id)?;
        validate_id(&self.provider_turn_id)?;
        if current_activation != &self.activation {
            return Err(HarnessProtocolError::StaleActivation);
        }
        if frame.len() > MAX_HARNESS_FRAME_BYTES {
            return Err(HarnessProtocolError::FrameTooLarge);
        }
        let message: Value = serde_json::from_slice(frame)?;
        let Some(method) = message.get("method").and_then(Value::as_str) else {
            // Acceptance responses require a host-owned request-id map. They cannot
            // be attributed from a turn-looking object alone.
            return Ok(HarnessObservation::Unattributed);
        };
        if message.get("id").is_some() {
            return Ok(HarnessObservation::RequiresAuthority {
                request_id: message["id"].clone(),
                method: bounded_string(method)?,
            });
        }
        let params = &message["params"];
        if params.get("threadId").and_then(Value::as_str) != Some(self.provider_thread_id.as_str())
        {
            return Ok(HarnessObservation::Unattributed);
        }
        let turn_id = params.get("turnId").and_then(Value::as_str).or_else(|| {
            params
                .get("turn")
                .and_then(|turn| turn.get("id"))
                .and_then(Value::as_str)
        });
        if turn_id != Some(self.provider_turn_id.as_str()) {
            return Ok(HarnessObservation::Unattributed);
        }
        Ok(match method {
            "turn/started" => HarnessObservation::Running,
            "turn/completed" => {
                let status = params["turn"]["status"]
                    .as_str()
                    .ok_or(HarnessProtocolError::MissingField("terminal status"))?;
                match status {
                    "completed" => HarnessObservation::Terminal(HarnessTerminal::Completed),
                    "interrupted" => HarnessObservation::Terminal(HarnessTerminal::Interrupted),
                    "failed" => HarnessObservation::Terminal(HarnessTerminal::Failed),
                    _ => HarnessObservation::Unattributed,
                }
            }
            "item/agentMessage/delta" => HarnessObservation::TextDelta {
                item_id: required_id(params, "itemId")?,
                text: params["delta"]
                    .as_str()
                    .ok_or(HarnessProtocolError::MissingField("delta"))?
                    .to_owned(),
            },
            "item/started" | "item/completed" => HarnessObservation::Item {
                item_id: required_id(&params["item"], "id")?,
                kind: bounded_string(
                    params["item"]["type"]
                        .as_str()
                        .ok_or(HarnessProtocolError::MissingField("item type"))?,
                )?,
                completed: method == "item/completed",
            },
            "thread/tokenUsage/updated" => HarnessObservation::Usage {
                input_tokens: params["tokenUsage"]["last"]["inputTokens"].as_u64(),
                output_tokens: params["tokenUsage"]["last"]["outputTokens"].as_u64(),
                // The event supplies token counts, not a billed dollar amount.
                cost_usd: None,
            },
            _ => HarnessObservation::Unattributed,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum HarnessObservation {
    Unattributed,
    RequiresAuthority {
        request_id: Value,
        method: String,
    },
    Running,
    Terminal(HarnessTerminal),
    TextDelta {
        item_id: String,
        text: String,
    },
    Item {
        item_id: String,
        kind: String,
        completed: bool,
    },
    Usage {
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
        cost_usd: Option<f64>,
    },
    /// Process loss is not a terminal outcome and never grants automatic redispatch.
    TransportLostOutcomeUnknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HarnessTerminal {
    Completed,
    Interrupted,
    Failed,
}

fn required_id(value: &Value, field: &'static str) -> Result<String, HarnessProtocolError> {
    let id = value
        .get(field)
        .and_then(Value::as_str)
        .ok_or(HarnessProtocolError::MissingField(field))?;
    validate_id(id)?;
    Ok(id.to_owned())
}

fn validate_id(value: &str) -> Result<(), HarnessProtocolError> {
    if value.is_empty()
        || value.len() > axocoatl_session::provider_run::MAX_PROVIDER_IDENTITY_BYTES
        || value.chars().any(char::is_control)
    {
        return Err(HarnessProtocolError::MissingField("bounded identity"));
    }
    Ok(())
}

fn bounded_string(value: &str) -> Result<String, HarnessProtocolError> {
    validate_id(value)?;
    Ok(value.to_owned())
}

#[cfg(test)]
#[path = "external_harness_tests.rs"]
mod tests;
