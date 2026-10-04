//! History reads through the exact retained controller synchronization.

use super::*;

impl SessionDispatchController {
    /// Canonical v2 uniqueness does not include sealed raw v1 IDs. Check the
    /// complete immutable frontier, including hidden rows, before retaining a
    /// new request or Begin. IDs are exact strings; never normalize legacy IDs.
    pub(crate) fn require_unoccupied_legacy_turn_id(
        canonical: &SessionExecutionStore,
        content: &ExecutionContentStore,
        turn_id: &LogicalTurnId,
    ) -> Result<()> {
        content.verify_canonical_owner(canonical).map_err(error)?;
        if let Some(seal) = canonical.legacy_seal().map_err(error)? {
            let legacy = content.read_legacy_history(&seal).map_err(error)?;
            if legacy.turns.iter().any(|turn| turn.id == turn_id.as_str()) {
                return Err(error(
                    "logical turn identity is occupied by retained legacy history",
                ));
            }
        }
        Ok(())
    }

    pub(crate) fn history_snapshot(
        &self,
    ) -> Result<axocoatl_session::session_history::SessionHistory> {
        let state = self.lock()?;
        state.ready()?;
        let mut history =
            axocoatl_session::session_history::SessionHistory::from_upgraded_with_commands(
                &state.canonical,
                &state.content,
                &state.turn_id,
                &state.commands,
            )
            .map_err(error)?;
        history
            .apply_superseded(&state.memory.superseded_turn_ids().map_err(error)?)
            .map_err(error)?;
        Ok(history)
    }
}

impl SessionDispatchController {
    /// The Session's network record namespace, for its single writer.
    pub(crate) fn network_record_namespace(
        &self,
    ) -> Result<axocoatl_session::execution_namespace::OwnedExecutionNamespace> {
        let state = self.lock()?;
        state.ready()?;
        crate::session_network::writer_namespace(&state.canonical).map_err(error)
    }

    /// Read the Session's network record without opening a writer.
    pub(crate) fn read_network_record(
        &self,
        after: Option<u64>,
        limit: usize,
        limits: axocoatl_session::network_record::RecordLimits,
    ) -> Result<
        Option<(
            Vec<axocoatl_session::network_record::NetworkLine>,
            axocoatl_session::network_record::RecordStats,
        )>,
    > {
        let state = self.lock()?;
        state.ready()?;
        crate::session_network::read_existing(&state.canonical, after, limit, limits).map_err(error)
    }

    /// Read one screenshot kept beside the Session's network record.
    pub(crate) fn read_network_screenshot(
        &self,
        sha256: &str,
    ) -> Result<Option<(String, Vec<u8>)>> {
        let state = self.lock()?;
        state.ready()?;
        crate::session_network::read_screenshot(&state.canonical, sha256).map_err(error)
    }

    /// Exact read through the same retained canonical owner. Earlier v2 turns
    /// and the sealed v1 frontier never fall back to a mutable legacy ledger.
    pub(crate) fn control_plane_for_turn(
        &self,
        turn_id: &str,
    ) -> Result<Option<crate::session_control_plane::SessionTurnControlPlane>> {
        let state = self.lock()?;
        // Legacy IDs were retained as exact strings before the v2 identity
        // rules existed. Parse only the canonical lookup, never its frontier.
        if let Ok(canonical_id) = LogicalTurnId::new(turn_id) {
            if state
                .canonical
                .turn(&canonical_id)
                .map_err(error)?
                .is_some()
            {
                let snapshot = state.canonical.snapshot(&canonical_id).map_err(error)?;
                return state
                    .project_control_plane(&snapshot, canonical_id == state.turn_id)
                    .map(Some);
            }
        }
        let Some(seal) = state.canonical.legacy_seal().map_err(error)? else {
            return Ok(None);
        };
        let superseded = state.memory.superseded_turn_ids().map_err(error)?;
        let frontier = state.content.read_legacy_history(&seal).map_err(error)?;
        Ok(frontier
            .turns
            .iter()
            .find(|turn| turn.id == turn_id)
            .map(|turn| {
                let mut view =
                    crate::session_control_plane::SessionTurnControlPlane::from_legacy(turn);
                view.mark_conversation_superseded(superseded.iter().any(|id| id == turn_id));
                view
            }))
    }
}

impl DispatchState {
    /// Shared current/historical read projection. Historical capability remains
    /// read-only even if its old receipt was Accepted or its usage is unknown.
    pub(super) fn project_control_plane(
        &self,
        snapshot: &DurableTurnSnapshot,
        current: bool,
    ) -> Result<crate::session_control_plane::SessionTurnControlPlane> {
        let mut view = crate::session_control_plane::SessionTurnControlPlane::from_execution(
            snapshot,
            &self.content,
        )
        .map_err(error)?;
        view.mark_conversation_superseded(
            self.memory
                .superseded_turn_ids()
                .map_err(error)?
                .iter()
                .any(|turn| turn == snapshot.turn_id().as_str()),
        );
        let audited = self
            .audit
            .turn_invocations(snapshot.turn_id())
            .map_err(error)?;
        join_invocation_evidence(&mut view, snapshot, audited)?;
        view.commands = if current {
            crate::session_control_plane::EvidenceValue::Available {
                value: self.control_plane_commands()?,
            }
        } else {
            match ControlCommandStore::read_historical_views(&self.canonical, snapshot.turn_id()) {
                Ok(value) => crate::session_control_plane::EvidenceValue::Available { value },
                Err(error) => crate::session_control_plane::EvidenceValue::Unavailable {
                    reason: error.to_string(),
                },
            }
        };
        if current {
            let time = now_ms()?;
            view.turn_controls = Some(self.human_turn_controls(time)?);
            for node in &mut view.nodes {
                for item in &mut node.activations {
                    if let crate::session_control_plane::ControlPlaneActivationRef::Exact {
                        activation,
                    } = &item.reference
                    {
                        item.capabilities = self.human_control_capabilities(activation, time)?;
                    }
                }
            }
        }
        join_web_sources(&self.canonical, &mut view);
        Ok(view)
    }
}

/// Join the Session network record's `web` events to activations as
/// `sources` evidence. The record is read without a lock or a writer; a
/// record that cannot be read leaves a warning instead of that evidence.
/// Only `web` lines are parsed, only those of this view's activations are
/// kept, and at the bound the newest are kept.
fn join_web_sources(
    canonical: &SessionExecutionStore,
    view: &mut crate::session_control_plane::SessionTurnControlPlane,
) {
    let activations = crate::session_dispatch_web::exact_activation_ids(view);
    if activations.is_empty() {
        return;
    }
    match axocoatl_session::network_record::NetworkRecord::read_existing_matching(
        canonical,
        axocoatl_session::network_record::RecordLimits::default(),
        "web",
        |event| crate::session_dispatch_web::is_web_event_for(event, &activations),
        crate::session_dispatch_web::projection_web_events_max(),
    ) {
        Ok(Some(lines)) => crate::session_dispatch_web::add_sources_evidence(view, &lines),
        Ok(None) => {}
        Err(error) => view.warnings.push(format!(
            "Web sources are unavailable: the network record could not be read: {error}"
        )),
    }
}

/// Both live and recovered reads preserve the same protected arguments, final
/// evidence and per-activation references. This function conveys no authority.
fn join_invocation_evidence(
    view: &mut crate::session_control_plane::SessionTurnControlPlane,
    snapshot: &DurableTurnSnapshot,
    audited_invocations: Vec<axocoatl_session::invocation_audit::AuditedInvocation>,
) -> Result<()> {
    let mut invocations = Vec::new();
    let mut observed = std::collections::HashSet::new();
    for audited in audited_invocations {
        let intent = &audited.intent;
        if &intent.activation.turn_id != snapshot.turn_id()
            || !observed.insert(intent.invocation_id.clone())
        {
            continue;
        }
        invocations.push(serde_json::json!({
                "invocation_id": intent.invocation_id, "activation": intent.activation,
                "intent": intent, "revision": audited.revision, "final_evidence": audited.final_evidence,
                "disposition": audited.disposition(), "scope": "durable_invocation_audit",
            }));
        for node in &mut view.nodes {
            for item in &mut node.activations {
                if !matches!(&item.reference, crate::session_control_plane::ControlPlaneActivationRef::Exact {activation} if activation == &intent.activation)
                {
                    continue;
                }
                item.evidence
                    .push(crate::session_control_plane::ControlPlaneEvidence {
                        kind: "tool_started".into(),
                        reference: crate::session_control_plane::EvidenceValue::Available {
                            value: intent.arguments.evidence_ref.as_str().into(),
                        },
                        summary: crate::session_control_plane::EvidenceValue::Available {
                            value: intent.redacted_preview.clone(),
                        },
                        recorded_at: crate::session_control_plane::EvidenceValue::NotRecorded,
                        details: crate::session_control_plane::EvidenceValue::Available {
                            value: serde_json::json!(intent),
                        },
                    });
                if let Some(final_evidence) = &audited.final_evidence {
                    use axocoatl_session::invocation_audit::InvocationFinalEvidence;
                    let (reference, summary) = match final_evidence {
                        InvocationFinalEvidence::Outcome {
                            result,
                            redacted_preview,
                            ..
                        } => (&result.evidence_ref, redacted_preview.clone()),
                        InvocationFinalEvidence::NotDispatched { evidence, .. } => {
                            (evidence, "Proven not dispatched".into())
                        }
                    };
                    item.evidence
                        .push(crate::session_control_plane::ControlPlaneEvidence {
                            kind: "tool_result".into(),
                            reference: crate::session_control_plane::EvidenceValue::Available {
                                value: reference.as_str().into(),
                            },
                            summary: crate::session_control_plane::EvidenceValue::Available {
                                value: summary,
                            },
                            recorded_at: crate::session_control_plane::EvidenceValue::NotRecorded,
                            details: crate::session_control_plane::EvidenceValue::Available {
                                value: serde_json::json!(final_evidence),
                            },
                        });
                }
            }
        }
    }
    for invocation in snapshot.contract().invocations() {
        if !observed.contains(&invocation.invocation_id) {
            invocations.push(serde_json::json!({"invocation_id": invocation.invocation_id,
                    "activation": invocation.activation, "evidence": invocation.evidence,
                    "disposition": invocation.evidence.disposition(), "scope":"canonical_turn_snapshot",
                    "audit":"not_recorded"}));
        }
    }
    view.invocations =
        crate::session_control_plane::EvidenceValue::Available { value: invocations };
    view.warnings
        .retain(|warning| !warning.starts_with("Invocation evidence reflects"));
    Ok(())
}

impl SessionDispatchController {
    pub(crate) fn project_retained_control_plane(
        canonical: &SessionExecutionStore,
        content: &ExecutionContentStore,
        snapshot: &DurableTurnSnapshot,
    ) -> Result<crate::session_control_plane::SessionTurnControlPlane> {
        content.verify_canonical_owner(canonical).map_err(error)?;
        let mut view = crate::session_control_plane::SessionTurnControlPlane::from_execution(
            snapshot, content,
        )
        .map_err(error)?;
        match InvocationAudit::read_retained_views(canonical, snapshot.turn_id()) {
            Ok(audited) => join_invocation_evidence(&mut view, snapshot, audited)?,
            Err(error) => view
                .warnings
                .push(format!("Retained invocation audit is unavailable: {error}")),
        }
        view.commands =
            match ControlCommandStore::read_retained_views(canonical, snapshot.turn_id()) {
                Ok(value) => crate::session_control_plane::EvidenceValue::Available { value },
                Err(error) => crate::session_control_plane::EvidenceValue::Unavailable {
                    reason: error.to_string(),
                },
            };
        view.expose_closed_turn_controls(snapshot)?;
        join_web_sources(canonical, &mut view);
        Ok(view)
    }
}
