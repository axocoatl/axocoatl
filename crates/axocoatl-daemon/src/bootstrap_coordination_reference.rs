//! Resolve composer references against owned retained history before Begin.
use super::{AxocoatlDaemon, SessionTurnContextReference, TurnContextScope};
use crate::error::DaemonError;
use crate::session_control_plane::{
    ControlPlaneActivationRef, EvidenceValue, SessionTurnControlPlane,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReferenceBinding {
    history_version: String,
    source_session_id: String,
    source_turn_id: String,
    node_id: String,
    generation: Option<u32>,
    reference_id: String,
    #[serde(rename = "type")]
    reference_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    execution_epoch_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    activation_id: Option<String>,
}

fn failure(message: impl Into<String>) -> DaemonError {
    DaemonError::Session(message.into())
}

impl ReferenceBinding {
    fn decode(
        reference: &SessionTurnContextReference,
        session_id: &str,
    ) -> Result<Self, DaemonError> {
        if reference.scope != TurnContextScope::ThisTurn
            || reference.content_sha256.is_some()
            || reference.origin.is_some()
        {
            return Err(failure(
                "coordination references require this-turn scope and server-resolved content",
            ));
        }
        let binding: Self = serde_json::from_value(Value::Object(reference.metadata.clone()))
            .map_err(|error| failure(format!("invalid coordination reference: {error}")))?;
        if binding.source_session_id != session_id
            || binding.reference_id != reference.reference_id
            || [
                &binding.source_turn_id,
                &binding.node_id,
                &binding.reference_id,
            ]
            .iter()
            .any(|value| value.is_empty() || value.len() > 1024)
            || !matches!(binding.reference_type.as_str(), "event" | "output")
        {
            return Err(failure(
                "coordination reference has an invalid or foreign source identity",
            ));
        }
        match binding.history_version.as_str() {
            "legacy_v1"
                if binding.execution_epoch_id.is_none() && binding.activation_id.is_none() => {}
            "execution_v2"
                if binding
                    .execution_epoch_id
                    .as_ref()
                    .is_some_and(|value| !value.is_empty())
                    && binding
                        .activation_id
                        .as_ref()
                        .is_some_and(|value| !value.is_empty())
                    && binding.generation.is_some() => {}
            _ => {
                return Err(failure(
                    "coordination reference lacks an exact versioned activation identity",
                ))
            }
        }
        Ok(binding)
    }

    fn resolve(&self, view: &SessionTurnControlPlane) -> Result<String, DaemonError> {
        if view.history_version != self.history_version
            || view.session_id != self.source_session_id
            || view.turn_id != self.source_turn_id
        {
            return Err(failure("coordination source version or owner changed"));
        }
        let node = view
            .nodes
            .iter()
            .find(|node| node.node_id == self.node_id)
            .ok_or_else(|| failure("coordination source node is unavailable"))?;
        let activation = node
            .activations
            .iter()
            .find(|activation| match &activation.reference {
                ControlPlaneActivationRef::Legacy { generation, .. } => {
                    self.history_version == "legacy_v1" && generation == &self.generation
                }
                ControlPlaneActivationRef::Exact { activation } => {
                    self.history_version == "execution_v2"
                        && Some(activation.generation) == self.generation
                        && Some(activation.execution_epoch_id.as_str())
                            == self.execution_epoch_id.as_deref()
                        && Some(activation.activation_id.as_str()) == self.activation_id.as_deref()
                }
            })
            .ok_or_else(|| failure("coordination source activation is unavailable"))?;
        let evidence = activation.evidence.iter().find(|evidence| matches!(&evidence.reference, EvidenceValue::Available {value} if value == &self.reference_id));
        if self.reference_type == "output" {
            if evidence.is_some_and(|evidence| {
                matches!(evidence.kind.as_str(), "agent_output" | "acceptance")
            }) {
                let body = if self.history_version == "legacy_v1" {
                    &evidence.expect("checked evidence").summary
                } else {
                    &activation.output
                };
                return match body {
                    EvidenceValue::Available { value } => Ok(value.clone()),
                    _ => Err(failure(
                        "complete source output is unavailable or exceeds the reference limit",
                    )),
                };
            }
            if let Some(output) = activation.partial_outputs.iter().find(|output| matches!(&output.reference, EvidenceValue::Available {value} if value == &self.reference_id)) {
                if !output.truncated { return Ok(output.text.clone()); }
            }
        } else if let Some(evidence) = evidence {
            if !matches!(evidence.kind.as_str(), "agent_output" | "acceptance") {
                return serde_json::to_string(evidence).map_err(|error| failure(error.to_string()));
            }
        }
        Err(failure("exact coordination evidence is unavailable"))
    }
}

impl AxocoatlDaemon {
    pub(super) async fn resolve_coordination_context(
        &self,
        session_id: &str,
        turn_id: &str,
        references: &[SessionTurnContextReference],
    ) -> Result<Vec<SessionTurnContextReference>, DaemonError> {
        let mut resolved = Vec::with_capacity(references.len());
        let existing = self
            .session_turn_store
            .lock()
            .await
            .get(turn_id)
            .filter(|turn| turn.session_id == session_id);
        for (index, reference) in references.iter().enumerate() {
            if reference.kind == "ways_decision" {
                resolved.push(
                    self.resolve_ways_decision_context(session_id, reference)
                        .await?,
                );
                continue;
            }
            if reference.kind != "coordination_reference" {
                resolved.push(reference.clone());
                continue;
            }
            let binding = ReferenceBinding::decode(reference, session_id)?;
            let context_id = format!("context:{turn_id}:{index}");
            // A reconnect reuses the already accepted bytes, even if its source
            // has subsequently been superseded or removed. A changed binding
            // still reaches Begin's complete idempotency comparison.
            if let Some(retained) = existing.as_ref().and_then(|turn| {
                turn.context.iter().find(|item| {
                    item.reference_id == context_id && item.kind == "coordination_reference"
                })
            }) {
                let mut metadata = retained.metadata.clone();
                metadata.remove("content");
                metadata.remove("superseded_conversation");
                if serde_json::from_value::<ReferenceBinding>(Value::Object(metadata))
                    .ok()
                    .as_ref()
                    == Some(&binding)
                {
                    resolved.push(retained.clone());
                    continue;
                }
            }
            let view = self
                .session_turn_control_plane(session_id, &binding.source_turn_id)
                .await?
                .ok_or_else(|| failure("coordination source turn is unavailable"))?;
            let body = binding.resolve(&view)?;
            let mut captured = reference.clone();
            captured.reference_id = context_id;
            captured.display_name = format!(
                "{} · {} · {}",
                binding.node_id, binding.reference_type, binding.source_turn_id
            );
            if view.superseded_conversation {
                captured.display_name.push_str(" · superseded conversation");
            }
            captured.media_type = Some("text/plain".into());
            captured.content_sha256 = Some(format!("{:x}", Sha256::digest(body.as_bytes())));
            captured.metadata = serde_json::to_value(&binding)
                .map_err(|error| failure(error.to_string()))?
                .as_object()
                .cloned()
                .ok_or_else(|| failure("invalid reference binding"))?;
            captured
                .metadata
                .insert("content".into(), Value::String(body));
            captured.metadata.insert(
                "superseded_conversation".into(),
                Value::Bool(view.superseded_conversation),
            );
            resolved.push(captured);
        }
        Ok(resolved)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axocoatl_session::turn_ledger::{
        BeginSessionTurn, SessionTurnAgentOutput, SessionTurnStore,
    };
    use serde_json::{json, Map};

    fn reference() -> SessionTurnContextReference {
        SessionTurnContextReference {
            reference_id: "output-first".into(),
            display_name: "Coder output".into(),
            kind: "coordination_reference".into(),
            scope: TurnContextScope::ThisTurn,
            origin: None,
            media_type: None,
            content_sha256: None,
            metadata: json!({"history_version":"legacy_v1", "source_session_id":"session-a",
                "source_turn_id":"source", "node_id":"coder", "generation":1,
                "reference_id":"output-first", "type":"output"})
            .as_object()
            .unwrap()
            .clone(),
        }
    }

    #[test]
    fn reference_uses_exact_retained_output_even_when_same_activation_has_newer_evidence() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = SessionTurnStore::open(dir.path()).unwrap();
        let mut turn = store
            .begin(BeginSessionTurn {
                turn_id: Some("source".into()),
                session_id: "session-a".into(),
                user_input: "request".into(),
                agent_id: Some("coder".into()),
                model: None,
                context: vec![],
                idempotency_key: None,
                metadata: Map::new(),
            })
            .unwrap();
        for (id, text) in [
            ("output-first", "original retained evidence"),
            ("output-second", "different later evidence"),
        ] {
            turn.agent_outputs.push(SessionTurnAgentOutput {
                operation_id: Some(id.into()),
                agent_id: "coder".into(),
                model: None,
                output: text.into(),
                attempt_id: None,
                activation_generation: Some(1),
                superseded: false,
                recorded_at: 1,
            });
        }
        let binding = ReferenceBinding::decode(&reference(), "session-a").unwrap();
        let view = SessionTurnControlPlane::from_legacy(&turn);
        assert_eq!(
            binding.resolve(&view).unwrap(),
            "original retained evidence"
        );
        turn.superseded = true;
        let rewound = SessionTurnControlPlane::from_legacy(&turn);
        assert!(rewound.superseded_conversation);
        assert_eq!(
            binding.resolve(&rewound).unwrap(),
            "original retained evidence"
        );
        assert!(rewound
            .warnings
            .iter()
            .any(|warning| warning.contains("conversation rewind")));
        turn.agent_outputs[0].superseded = true;
        assert_eq!(
            binding
                .resolve(&SessionTurnControlPlane::from_legacy(&turn))
                .unwrap(),
            "original retained evidence"
        );
        let mut changed = binding.clone();
        changed.generation = Some(2);
        assert!(changed.resolve(&view).is_err());
        changed = binding;
        changed.reference_id = "missing".into();
        assert!(changed.resolve(&view).is_err());
    }

    #[test]
    fn composer_cannot_supply_reference_text_hash_authority_or_foreign_owner() {
        let mut source = reference();
        assert!(ReferenceBinding::decode(&source, "session-b").is_err());
        source
            .metadata
            .insert("content".into(), json!("forged provider input"));
        assert!(ReferenceBinding::decode(&source, "session-a").is_err());
        source.metadata.remove("content");
        source.content_sha256 = Some("forged".into());
        assert!(ReferenceBinding::decode(&source, "session-a").is_err());
        source.content_sha256 = None;
        source
            .metadata
            .insert("history_version".into(), json!("execution_v2"));
        assert!(ReferenceBinding::decode(&source, "session-a").is_err());
    }

    #[test]
    fn canonical_reference_body_is_the_same_model_input_after_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = SessionTurnStore::open(dir.path()).unwrap();
        let mut source = reference();
        source
            .metadata
            .insert("content".into(), json!("exact reviewer evidence"));
        source.content_sha256 = Some(format!("{:x}", Sha256::digest(b"exact reviewer evidence")));
        let turn = store
            .begin(BeginSessionTurn {
                turn_id: Some("followup".into()),
                session_id: "session-a".into(),
                user_input: "address this finding".into(),
                agent_id: Some("coder".into()),
                model: None,
                context: vec![source],
                idempotency_key: None,
                metadata: Map::new(),
            })
            .unwrap();
        let before = axocoatl_memory::legacy_conversation::checkpoint_user_content(&turn);
        drop(store);
        let reopened = SessionTurnStore::open(dir.path())
            .unwrap()
            .get("followup")
            .unwrap();
        assert_eq!(
            before,
            axocoatl_memory::legacy_conversation::checkpoint_user_content(&reopened)
        );
        let replay = axocoatl_memory::legacy_conversation::project_history(
            &|text| text.len(),
            &[reopened],
            axocoatl_memory::legacy_conversation::ToolReplayPolicy::CompleteNativeGroups,
        );
        assert_eq!(replay[0].content, before);
        assert!(before.contains("exact reviewer evidence"));
        assert!(before.ends_with("address this finding"));
    }
}
