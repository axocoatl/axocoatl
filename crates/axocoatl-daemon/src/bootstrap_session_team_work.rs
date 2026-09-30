//! Standing sources converge on the ordinary canonical Session controller.
use super::native_send::NativeSessionSend;
use super::native_turn::{NativeFirstTurnRequest, NativeFirstTurnStart};
use super::*;
use axocoatl_session::control_authority::ControlAuthority;
use axocoatl_session::execution_content::ActivationEvidenceContent;
use axocoatl_session::execution_namespace::ExecutionComponent;
use axocoatl_session::session_history::SessionHistoryEntry;
use axocoatl_session::session_team::SessionTeamStore;
use axocoatl_session::team_work::SettlementBasis;
pub use axocoatl_session::team_work::{
    ArmedTeamWorkBinding, TeamWorkEvent, TeamWorkReceipt, TeamWorkSource, TeamWorkSubject,
};
use axocoatl_session::team_work::{
    TeamWorkBinding, TeamWorkDisposition, TeamWorkGrantReference, TeamWorkInbox, TeamWorkRequest,
};
use axocoatl_session::turn_contract::{LogicalTurnId, LogicalTurnState};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionWorkBindingEdit {
    pub binding_id: String,
    pub expected_binding_revision: u64,
    pub expected_team_revision: u64,
    pub armed: bool,
    pub source: TeamWorkSource,
    pub event_kind: String,
    pub instruction: String,
    #[serde(default)]
    pub required_checks: Vec<Vec<String>>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionWorkEventInput {
    #[serde(default)]
    pub expected_binding_revision: Option<u64>,
    pub event_id: String,
    pub correlation_id: String,
    pub subject: TeamWorkSubject,
    pub evidence_refs: Vec<String>,
    pub caused_by_turn_id: Option<String>,
}
#[path = "bootstrap_session_work_causal.rs"]
mod causal;
#[path = "bootstrap_session_work_readiness.rs"]
mod readiness;
#[path = "bootstrap_session_work_signals.rs"]
mod signals;
pub use readiness::SessionWorkReadiness;
pub use signals::{
    SignalDepositView, SignalDispatchView, SignalFieldView, SignalFlagInput, SignalSensorView,
    SignalWithdrawInput,
};
#[derive(Serialize)]
pub struct SessionWorkItem {
    pub receipt: TeamWorkReceipt,
    pub state: String,
    pub reason: Option<String>,
    pub readiness: SessionWorkReadiness,
    pub can_dismiss: bool,
    /// What settling this closed work at its reserved ceiling would charge,
    /// when unknown provider usage is all that holds its budget.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ceiling: Option<CeilingPreview>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CeilingPreview {
    pub tokens: u64,
    pub cost_microunits: u64,
    pub unknown_calls: u32,
}
#[derive(Serialize)]
pub struct SessionWorkView {
    pub bindings: Vec<ArmedTeamWorkBinding>,
    pub receipts: Vec<SessionWorkItem>,
}

fn work_error(error: impl std::fmt::Display) -> DaemonError {
    DaemonError::SessionConflict(error.to_string())
}
fn now_ms() -> Result<u64, DaemonError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(work_error)?
        .as_millis()
        .try_into()
        .map_err(work_error)
}

impl AxocoatlDaemon {
    fn work_inbox(&self) -> Result<std::sync::MutexGuard<'_, TeamWorkInbox>, DaemonError> {
        self.session_team_work
            .lock()
            .map_err(|_| work_error("Standing work storage requires recovery"))
    }

    pub async fn configure_session_work(
        &self,
        session_id: &str,
        edit: SessionWorkBindingEdit,
    ) -> Result<ArmedTeamWorkBinding, DaemonError> {
        self.require_runtime_admission()?;
        {
            let inbox = self.work_inbox()?;
            if let Some(saved) = inbox.bindings().map_err(work_error)?.iter().find(|saved| {
                saved.binding.binding_id == edit.binding_id
                    && saved.binding.binding_revision
                        == edit.expected_binding_revision.saturating_add(1)
            }) {
                if saved.binding.session_id == session_id
                    && saved.binding.team_revision == edit.expected_team_revision
                    && saved.source == edit.source
                    && saved.armed == edit.armed
                    && saved.instruction == edit.instruction
                    && saved.required_checks == edit.required_checks
                    && saved.binding.event_kind == edit.event_kind
                {
                    return Ok(saved.clone());
                }
                return Err(work_error(
                    "This binding revision already records a different decision",
                ));
            }
        }
        if !edit.armed {
            let mut inbox = self.work_inbox()?;
            if let Some(saved) = inbox
                .current_binding(&edit.binding_id)
                .map_err(work_error)?
                .cloned()
            {
                if saved.binding.session_id != session_id {
                    return Err(work_error("Work source belongs to another Session"));
                }
                if saved.source == edit.source
                    && saved.binding.team_revision == edit.expected_team_revision
                    && saved.binding.event_kind == edit.event_kind
                    && saved.instruction == edit.instruction
                    && saved.required_checks == edit.required_checks
                {
                    let mut disarmed = saved;
                    disarmed.armed = false;
                    disarmed.binding.binding_revision = edit
                        .expected_binding_revision
                        .checked_add(1)
                        .ok_or_else(|| work_error("Binding revision exhausted"))?;
                    disarmed.authorized_at_ms = now_ms()?;
                    return inbox
                        .configure_binding(edit.expected_binding_revision, disarmed)
                        .map_err(work_error);
                }
            }
        }
        let session = self
            .get_session(session_id)
            .await
            .ok_or_else(|| work_error("Session not found"))?;
        if session.status == axocoatl_session::SessionStatus::Closed {
            return Err(work_error("Reopen the Session before arming work"));
        }
        self.verify_work_source(&edit.source)?;
        let source_after_turn = if let TeamWorkSource::SessionCompletion { session_id: source } =
            &edit.source
        {
            let source_session = self
                .get_session(source)
                .await
                .ok_or_else(|| work_error("Source Session not found"))?;
            if source_session.workspace_id != session.workspace_id {
                return Err(work_error("Choose a source Session in the same Workspace"));
            }
            let source_history = self
                .session_dispatch_lifecycles
                .history_snapshot(source)?
                .ok_or_else(|| work_error("The source Session has no canonical history"))?;
            source_history.entries(axocoatl_session::session_history::HistoryVisibility::IncludingSuperseded).into_iter().rev().find(|entry|matches!(entry,SessionHistoryEntry::ExecutionV2(turn) if turn.state.is_closed())).map(|entry|entry.turn_id().to_owned())
        } else {
            None
        };
        let token = self
            .session_dispatch_lifecycles
            .session_team_token(session_id)?;
        let binding = self.session_dispatch_lifecycles.with_session_team_stores(
            &token,
            |canonical, content, _| {
                let team = SessionTeamStore::open_owned(
                    canonical
                        .component_namespace(ExecutionComponent::SessionTeam)
                        .map_err(work_error)?,
                    canonical,
                    content,
                    None,
                )
                .map_err(work_error)?;
                let current = team
                    .current()
                    .map_err(work_error)?
                    .ok_or_else(|| work_error("Apply Team and budget before arming a source"))?;
                if current.configuration_revision != edit.expected_team_revision {
                    return Err(work_error(
                        "The Session team changed; review the current team before arming work",
                    ));
                }
                let mut grants = Vec::new();
                for slot in &current.graph.slots {
                    let reference = slot
                        .grant
                        .as_ref()
                        .ok_or_else(|| work_error("Every Agent requires an approved grant"))?;
                    let ActivationEvidenceContent::Grant { policy } = content
                        .resolve_activation_evidence(reference)
                        .map_err(work_error)?
                    else {
                        return Err(work_error("The approved grant evidence is unavailable"));
                    };
                    if edit.armed && now_ms()? >= policy.expires_at_ms {
                        return Err(work_error("The approved grant has expired"));
                    }
                    grants.push(TeamWorkGrantReference {
                        id: policy.id.clone(),
                        revision: policy.revision,
                        limits: policy.limits.clone(),
                        expires_at_ms: policy.expires_at_ms,
                    });
                }
                let primary = grants
                    .first()
                    .ok_or_else(|| work_error("The Session team is empty"))?;
                let source_id = match &edit.source {
                    TeamWorkSource::Manual => "manual".to_owned(),
                    TeamWorkSource::SignedWebhook { configuration_name } => {
                        format!("webhook:{configuration_name}")
                    }
                    TeamWorkSource::SessionCompletion { session_id } => {
                        format!("session:{session_id}")
                    }
                    TeamWorkSource::SignalField { .. } => signals::SIGNAL_SOURCE_ID.to_owned(),
                };
                let slots: Vec<String> = current
                    .graph
                    .slots
                    .iter()
                    .map(|slot| slot.slot_id.as_str().to_owned())
                    .collect();
                self.validate_signal_routes(session_id, &edit.source, &slots)?;
                let binding = ArmedTeamWorkBinding {
                    binding: TeamWorkBinding {
                        binding_id: edit.binding_id,
                        binding_revision: edit
                            .expected_binding_revision
                            .checked_add(1)
                            .ok_or_else(|| work_error("Binding revision exhausted"))?,
                        workspace_id: session.workspace_id.clone(),
                        session_id: session_id.to_owned(),
                        team_revision: current.configuration_revision,
                        grant_id: primary.id.clone(),
                        grant_revision: primary.revision,
                        source_id,
                        event_kind: edit.event_kind,
                    },
                    source: edit.source,
                    armed: edit.armed,
                    instruction: edit.instruction,
                    required_checks: edit.required_checks,
                    grants,
                    authorized_at_ms: now_ms()?,
                    source_after_turn,
                };
                self.work_inbox()?
                    .configure_binding(edit.expected_binding_revision, binding)
                    .map_err(work_error)
            },
        )?;
        // A newly armed field records what already exists before it can sense.
        self.arm_signal_field(&binding).await?;
        Ok(binding)
    }

    fn verify_work_source(&self, source: &TeamWorkSource) -> Result<(), DaemonError> {
        if let TeamWorkSource::SignedWebhook { configuration_name } = source {
            let matches: Vec<_> = self
                .config
                .webhooks
                .iter()
                .filter(|hook| &hook.name == configuration_name)
                .collect();
            if matches.len() != 1
                || !matches[0].enabled
                || matches[0]
                    .secret
                    .as_ref()
                    .is_none_or(|secret| secret.is_empty())
            {
                return Err(work_error(
                    "Select one enabled configured webhook with a signing secret",
                ));
            }
        }
        Ok(())
    }

    fn work_binding(
        &self,
        session_id: &str,
        binding_id: &str,
    ) -> Result<ArmedTeamWorkBinding, DaemonError> {
        let binding = self
            .work_inbox()?
            .current_binding(binding_id)
            .map_err(work_error)?
            .cloned()
            .ok_or_else(|| work_error("Work source not found"))?;
        if binding.binding.session_id != session_id {
            return Err(work_error("Work source belongs to another Session"));
        }
        Ok(binding)
    }

    pub async fn admit_manual_session_work(
        &self,
        session_id: &str,
        binding_id: &str,
        input: SessionWorkEventInput,
    ) -> Result<TeamWorkReceipt, DaemonError> {
        let binding = self.work_binding(session_id, binding_id)?;
        if binding.source != TeamWorkSource::Manual {
            return Err(work_error(
                "This source requires a verified producer signature",
            ));
        }
        let bytes = serde_json::to_vec(&input).map_err(work_error)?;
        self.admit_authenticated_work(session_id, binding, input, &bytes)
            .await
    }

    pub async fn admit_signed_session_work(
        &self,
        session_id: &str,
        binding_id: &str,
        raw_body: &[u8],
        signature: &str,
    ) -> Result<TeamWorkReceipt, DaemonError> {
        if raw_body.len() > 32 * 1024 {
            return Err(work_error("Work event exceeds 32 KiB"));
        }
        let binding = self.work_binding(session_id, binding_id)?;
        self.verify_work_source(&binding.source)?;
        let TeamWorkSource::SignedWebhook { configuration_name } = &binding.source else {
            return Err(work_error("This work source is manual"));
        };
        let hook = self
            .config
            .webhooks
            .iter()
            .find(|hook| &hook.name == configuration_name)
            .ok_or_else(|| work_error("Producer configuration is unavailable"))?;
        let secret = hook
            .secret
            .as_ref()
            .ok_or_else(|| work_error("Producer signing secret is unavailable"))?;
        verify_signature(secret.expose_secret().as_bytes(), raw_body, signature)?;
        let input = serde_json::from_slice(raw_body).map_err(work_error)?;
        self.admit_authenticated_work(session_id, binding, input, raw_body)
            .await
    }

    async fn admit_authenticated_work(
        &self,
        session_id: &str,
        binding: ArmedTeamWorkBinding,
        input: SessionWorkEventInput,
        authenticated_bytes: &[u8],
    ) -> Result<TeamWorkReceipt, DaemonError> {
        self.require_runtime_admission()?;
        if authenticated_bytes.len() > 32 * 1024 {
            return Err(work_error("Work event exceeds 32 KiB"));
        }
        let session = self
            .get_session(session_id)
            .await
            .ok_or_else(|| work_error("Session not found"))?;
        if session.workspace_id != binding.binding.workspace_id {
            return Err(work_error("The source Workspace changed"));
        }
        // External payloads cannot manufacture Axocoatl causation. The internal
        // artifact producer must supply an opaque verified join for this field.
        if input.caused_by_turn_id.is_some() {
            return Err(work_error("A producer-supplied turn ID is not verified causation; use retained candidate evidence"));
        }
        let expected_binding_revision = input.expected_binding_revision;
        let event = TeamWorkEvent {
            source_id: binding.binding.source_id.clone(),
            event_id: input.event_id,
            event_kind: binding.binding.event_kind.clone(),
            content_sha256: format!("{:x}", Sha256::digest(authenticated_bytes)),
            correlation_id: input.correlation_id,
            caused_by_turn_id: None,
            subject: input.subject,
            evidence_refs: input.evidence_refs,
        };
        let mut inbox = self.work_inbox()?;
        if let Some(existing) = inbox
            .receipts()
            .map_err(work_error)?
            .iter()
            .find(|receipt| {
                receipt.request.binding.binding_id == binding.binding.binding_id
                    && receipt.request.event.source_id == event.source_id
                    && receipt.request.event.event_id == event.event_id
            })
        {
            return if existing.request.event == event {
                Ok(existing.clone())
            } else {
                Err(work_error(
                    "This event was already admitted with different content",
                ))
            };
        }
        if expected_binding_revision
            .is_some_and(|revision| revision != binding.binding.binding_revision)
        {
            return Err(work_error(
                "The work source changed; review its current binding before submitting",
            ));
        }
        inbox
            .admit_bound(
                TeamWorkRequest {
                    binding: binding.binding,
                    event,
                },
                now_ms()?,
            )
            .map_err(work_error)
    }

    /// Dry-run settlement at the reserved ceiling for closed work whose budget
    /// is held only by provider calls with unknown usage.
    fn work_ceiling_preview(
        &self,
        receipt: &TeamWorkReceipt,
    ) -> Result<Option<CeilingPreview>, DaemonError> {
        if receipt.disposition != TeamWorkDisposition::Reserved
            || receipt.ceiling_decision.is_some()
            || receipt
                .allocations
                .iter()
                .all(|allocation| allocation.is_settled())
        {
            return Ok(None);
        }
        let allocations = self
            .work_inbox()?
            .native_allocations(&receipt.receipt_id)
            .map_err(work_error)?;
        let id = LogicalTurnId::new(&receipt.turn_id).map_err(work_error)?;
        let token = self
            .session_dispatch_lifecycles
            .session_team_token(&receipt.request.binding.session_id)?;
        let Some(settled) = self
            .session_dispatch_lifecycles
            .with_session_team_settlement_stores(&token, |canonical, held| {
                settle_work_allocations(canonical, held, &id, &allocations, true)
            })?
        else {
            return Ok(None);
        };
        // A total past the grant's reviewed limits (after an expansion) cannot
        // be recorded; the work stays reserved instead.
        if self
            .work_inbox()?
            .check_native_settlement(&receipt.receipt_id, &settled)
            .is_err()
        {
            return Ok(None);
        }
        // Settlement is all or nothing, so the figure covers every grant this
        // work charges, measured ones included.
        let mut preview = CeilingPreview {
            tokens: 0,
            cost_microunits: 0,
            unknown_calls: 0,
        };
        for settlement in &settled {
            if let SettlementBasis::ReservedCeiling { unknown_calls } = settlement.basis() {
                preview.unknown_calls += unknown_calls;
            }
            let (before, after) = (settlement.consumed_before(), settlement.total_consumed());
            preview.tokens += after.tokens.saturating_sub(before.tokens);
            preview.cost_microunits += after.cost_microunits.saturating_sub(before.cost_microunits);
        }
        Ok((preview.unknown_calls > 0).then_some(preview))
    }

    /// A person accepts charging this closed work's unknown provider usage at
    /// its full admitted reservation, which releases the rest of the shared
    /// budget. Refused unless unknown provider usage is all that holds it.
    pub async fn settle_session_work_at_ceiling(
        &self,
        session_id: &str,
        receipt_id: &str,
    ) -> Result<SessionWorkView, DaemonError> {
        {
            let _runner = self.session_team_work_runner.lock().await;
            let receipt = self.work_receipt(session_id, receipt_id)?;
            // A resend after a lost reply finds the decision already recorded.
            if receipt.ceiling_decision.is_none() {
                if self.work_ceiling_preview(&receipt)?.is_none() {
                    return Err(work_error(
                        "This work cannot be settled at its ceiling: its turn is still open, a \
                         tool effect or check outcome is unknown, its usage is already measured, \
                         or the ceiling would exceed the grant's reviewed limits",
                    ));
                }
                self.work_inbox()?
                    .record_ceiling_decision(receipt_id, now_ms()?)
                    .map_err(work_error)?;
            }
            self.reconcile_session_work(session_id)?;
        }
        self.session_work(session_id).await
    }

    fn work_receipt(
        &self,
        session_id: &str,
        receipt_id: &str,
    ) -> Result<TeamWorkReceipt, DaemonError> {
        self.work_inbox()?
            .receipts()
            .map_err(work_error)?
            .iter()
            .find(|receipt| {
                receipt.receipt_id == receipt_id && receipt.request.binding.session_id == session_id
            })
            .cloned()
            .ok_or_else(|| work_error("Work receipt not found"))
    }

    pub async fn dismiss_session_work(
        &self,
        session_id: &str,
        receipt_id: &str,
        reason: String,
    ) -> Result<TeamWorkReceipt, DaemonError> {
        let _runner = self.session_team_work_runner.lock().await;
        let receipt = self.work_receipt(session_id, receipt_id)?;
        if receipt.disposition == TeamWorkDisposition::Reserved || receipt.never_begun.is_some() {
            let token = self
                .session_dispatch_lifecycles
                .session_team_token(session_id)?;
            return self.session_dispatch_lifecycles.with_session_team_stores(
                &token,
                |canonical, _, _| {
                    self.work_inbox()?
                        .dismiss_native_never_begun(receipt_id, canonical, reason)
                        .map_err(work_error)
                },
            );
        }
        self.work_inbox()?
            .dismiss(receipt_id, reason)
            .map_err(work_error)
    }

    pub async fn session_work(&self, session_id: &str) -> Result<SessionWorkView, DaemonError> {
        self.reconcile_session_work(session_id)?;
        let history = self
            .session_dispatch_lifecycles
            .history_snapshot(session_id)?
            .ok_or_else(|| work_error("Retained Session history is unavailable"))?;
        let inbox = self.work_inbox()?;
        let mut bindings = Vec::new();
        for binding in inbox
            .bindings()
            .map_err(work_error)?
            .iter()
            .filter(|binding| binding.binding.session_id == session_id)
        {
            if inbox
                .current_binding(&binding.binding.binding_id)
                .map_err(work_error)?
                == Some(binding)
            {
                bindings.push(binding.clone());
            }
        }
        let saved_bindings = inbox.bindings().map_err(work_error)?.to_vec();
        let saved_receipts = inbox.receipts().map_err(work_error)?.to_vec();
        drop(inbox);
        let receipts = saved_receipts.iter().filter(|receipt| receipt.request.binding.session_id == session_id).map(|receipt| {
            let (mut state, mut reason) = match &receipt.disposition {
                TeamWorkDisposition::Dismissed { reason } => ("dismissed".into(), Some(reason.clone())),
                TeamWorkDisposition::Queued => match bindings.iter().find(|binding|binding.binding.binding_id == receipt.request.binding.binding_id) {
                    Some(binding) if binding.armed && binding.binding == receipt.request.binding => ("queued".into(), None),
                    _ => ("blocked".into(), Some("The source was disarmed or changed; review or dismiss this event".into())),
                },
                TeamWorkDisposition::Reserved => match history.get(&receipt.turn_id) {
                    Some(SessionHistoryEntry::ExecutionV2(turn)) if turn.state.is_closed() && receipt.allocations.iter().all(|allocation| allocation.is_settled()) => ("settled".into(), None),
                    Some(SessionHistoryEntry::ExecutionV2(turn)) => (format!("{:?}", turn.state).to_lowercase(), if turn.state.is_closed() { Some("Measured usage or effect settlement is incomplete; shared budget remains reserved".into()) } else { None }),
                    _ => ("reserved".into(), Some("The exact request is retained; execution has not been admitted".into())),
                },
            };
            if receipt.blocked_reason.is_some() && matches!(state.as_str(),"queued"|"reserved") { state = "blocked".into(); reason = receipt.blocked_reason.clone(); }
            let original = saved_bindings.iter().find(|binding|binding.binding == receipt.request.binding);
            let current = bindings.iter().find(|binding|binding.binding.binding_id == receipt.request.binding.binding_id);
            let ceiling = if matches!(history.get(&receipt.turn_id), Some(SessionHistoryEntry::ExecutionV2(turn)) if turn.state.is_closed()) { self.work_ceiling_preview(receipt)? } else { None };
            if let Some(preview) = &ceiling {
                let cost = if preview.cost_microunits > 0 { format!(" and {} cost microunits", preview.cost_microunits) } else { String::new() };
                reason = Some(format!("Provider usage is unknown for {} call(s); the shared budget stays reserved until a person settles it at its reserved ceiling, which charges this work {} tokens{cost} in total", preview.unknown_calls, preview.tokens));
            }
            Ok(SessionWorkItem { receipt: receipt.clone(), state, reason, readiness:self.work_readiness(receipt, original, current)?, can_dismiss:receipt.disposition == TeamWorkDisposition::Queued || (receipt.disposition == TeamWorkDisposition::Reserved && history.get(&receipt.turn_id).is_none()), ceiling })
        }).collect::<Result<Vec<_>, DaemonError>>()?;
        Ok(SessionWorkView { bindings, receipts })
    }

    fn reconcile_session_work(&self, session_id: &str) -> Result<(), DaemonError> {
        let receipts: Vec<_> = self
            .work_inbox()?
            .receipts()
            .map_err(work_error)?
            .iter()
            .filter(|receipt| {
                receipt.request.binding.session_id == session_id
                    && receipt.disposition == TeamWorkDisposition::Reserved
                    && receipt
                        .allocations
                        .iter()
                        .any(|allocation| !allocation.is_settled())
            })
            .cloned()
            .collect();
        if receipts.is_empty() {
            return Ok(());
        }
        let token = self
            .session_dispatch_lifecycles
            .session_team_token(session_id)?;
        for receipt in receipts {
            let allocations = self
                .work_inbox()?
                .native_allocations(&receipt.receipt_id)
                .map_err(work_error)?;
            let id = LogicalTurnId::new(&receipt.turn_id).map_err(work_error)?;
            let at_ceiling = receipt.ceiling_decision.is_some();
            let settled = self
                .session_dispatch_lifecycles
                .with_session_team_settlement_stores(&token, |canonical, held| {
                    settle_work_allocations(canonical, held, &id, &allocations, at_ceiling)
                })?;
            if let Some(settled) = settled {
                self.work_inbox()?
                    .settle_native_budget(&receipt.receipt_id, &settled)
                    .map_err(work_error)?;
            }
        }
        Ok(())
    }

    pub async fn run_session_work(
        &self,
        session_id: &str,
        receipt_id: &str,
    ) -> Result<SessionWorkView, DaemonError> {
        let _runner = self.session_team_work_runner.lock().await;
        self.reconcile_session_work(session_id)?;
        let mut receipt = self.work_receipt(session_id, receipt_id)?;
        self.work_inbox()?
            .record_blocked(receipt_id, None)
            .map_err(work_error)?;
        if let TeamWorkDisposition::Dismissed { .. } = receipt.disposition {
            return self.session_work(session_id).await;
        }
        let history = self
            .session_dispatch_lifecycles
            .history_snapshot(session_id)?
            .ok_or_else(|| work_error("Retained Session history is unavailable"))?;
        if history.get(&receipt.turn_id).is_some() {
            return self.session_work(session_id).await;
        }
        {
            let inbox = self.work_inbox()?;
            if inbox
                .receipts()
                .map_err(work_error)?
                .iter()
                .take_while(|item| item.receipt_id != receipt_id)
                .any(|item| {
                    item.request.binding.session_id == session_id
                        && !matches!(item.disposition, TeamWorkDisposition::Dismissed { .. })
                        && (item.disposition == TeamWorkDisposition::Queued
                            || item
                                .allocations
                                .iter()
                                .any(|allocation| !allocation.is_settled()))
                })
            {
                return Err(work_error(
                    "Earlier work is waiting or unresolved; review that event first",
                ));
            }
        }
        let binding = self.work_binding(session_id, &receipt.request.binding.binding_id)?;
        if !binding.armed || binding.binding != receipt.request.binding {
            return Err(work_error(
                "This event's source binding was disarmed or changed",
            ));
        }
        self.verify_work_source(&binding.source)?;
        let request = if let Some(source) = &receipt.execution_source {
            serde_json::from_str::<NativeFirstTurnRequest>(source).map_err(work_error)?
        } else {
            // A signal is rechecked against current source before it starts and
            // goes only to the Agent responsible for the signaled paths.
            let (input, display_input, target_agent, write_scope, signal_routes) =
                if matches!(binding.source, TeamWorkSource::SignalField { .. }) {
                    match self.prepare_signal_execution(&binding, &receipt).await? {
                        signals::SignalExecution::Run {
                            target,
                            input,
                            display,
                            write_scope,
                            routes,
                        } => (input, display, Some(target), Some(write_scope), routes),
                        signals::SignalExecution::Superseded(reason) => {
                            self.work_inbox()?
                                .dismiss(receipt_id, reason)
                                .map_err(work_error)?;
                            return self.session_work(session_id).await;
                        }
                    }
                } else {
                    (
                        format!(
                            "{}\n\nDeclared work candidate (verify before claiming readiness):\n{}",
                            binding.instruction,
                            serde_json::to_string(&receipt.request.event).map_err(work_error)?
                        ),
                        format!(
                            "{}: {}",
                            binding.binding.event_kind, receipt.request.event.subject.reference_id
                        ),
                        None,
                        None,
                        Vec::new(),
                    )
                };
            let mut request = self
                .prepare_native_send_request(&NativeSessionSend {
                    session_id: session_id.to_owned(),
                    turn_id: receipt.turn_id.clone(),
                    idempotency_key: Some(receipt.receipt_id.clone()),
                    display_input: Some(display_input),
                    input,
                    reference_ids: Vec::new(),
                    context_references: Vec::new(),
                    model_override: None,
                    target_agent: target_agent.clone(),
                })
                .await?;
            let requested: Vec<_> = request
                .grants
                .iter()
                .map(|grant| TeamWorkGrantReference {
                    id: grant.id.clone(),
                    revision: grant.revision,
                    limits: grant.limits.clone(),
                    expires_at_ms: grant.expires_at_ms,
                })
                .collect();
            // Whole-team work uses every approved grant. Targeted signal work
            // uses exactly its Agent's grant, unchanged from the binding.
            let exact = if target_agent.is_some() {
                !requested.is_empty()
                    && requested.iter().all(|grant| binding.grants.contains(grant))
            } else {
                requested == binding.grants
            };
            if request.expected_team_revision != binding.binding.team_revision || !exact {
                return Err(work_error(
                    "The approved team or standing allowance changed",
                ));
            }
            request.standing_work = Some(super::native_turn::NativeStandingWork {
                receipt_id: receipt.receipt_id.clone(),
                binding: binding.binding.clone(),
                subject: receipt.request.event.subject.clone(),
                required_checks: binding.required_checks.clone(),
                write_scope: write_scope.clone(),
                signal_routes: signal_routes.clone(),
            });
            receipt = if target_agent.is_some() {
                let grants: Vec<String> = requested.iter().map(|grant| grant.id.clone()).collect();
                self.work_inbox()?.reserve_native_turn_for_grants(
                    receipt_id,
                    request.source()?,
                    &grants,
                )
            } else {
                self.work_inbox()?
                    .reserve_native_turn(receipt_id, request.source()?)
            }
            .map_err(work_error)?;
            request
        };
        self.validate_standing_work_admission(session_id, &receipt.turn_id, &request.source()?)?;
        match self.prepare_native_turn(request).await? {
            NativeFirstTurnStart::Prepared(prepared) => {
                let controller = prepared.controller();
                let result = prepared.run().await;
                let history = controller.history_snapshot().map_err(work_error)?;
                if let Some(SessionHistoryEntry::ExecutionV2(turn)) = history.get(&receipt.turn_id)
                {
                    let mut usage = axocoatl_core::MeasuredTokenUsage::known(
                        axocoatl_core::TokenUsageStats::default(),
                    );
                    for activation in &turn.activations {
                        match controller
                            .activation_provider_usage(&activation.activation.activation)
                        {
                            Ok(measured) => {
                                usage.usage.merge(&measured.tokens.usage);
                                usage.complete &= measured.tokens.complete;
                            }
                            Err(_) => usage.complete = false,
                        }
                    }
                    super::native_send::publish_native_disposition(&self.stream_bus, turn, &usage);
                }
                self.reconcile_session_work(session_id)?;
                result?;
            }
            NativeFirstTurnStart::Reattached(_) => {}
        }
        self.session_work(session_id).await
    }

    pub(super) fn validate_standing_work_admission(
        &self,
        session_id: &str,
        turn_id: &str,
        source: &str,
    ) -> Result<(), DaemonError> {
        let inbox = self.work_inbox()?;
        let Some(receipt) = inbox
            .receipts()
            .map_err(work_error)?
            .iter()
            .find(|receipt| {
                receipt.request.binding.session_id == session_id && receipt.turn_id == turn_id
            })
        else {
            let value: serde_json::Value = serde_json::from_str(source).map_err(work_error)?;
            if value
                .get("standing_work")
                .is_some_and(|work| !work.is_null())
            {
                return Err(work_error("Standing work has no durable admission receipt"));
            }
            return Ok(());
        };
        let binding = inbox
            .current_binding(&receipt.request.binding.binding_id)
            .map_err(work_error)?
            .ok_or_else(|| work_error("Standing source binding is missing"))?;
        let now = now_ms()?;
        if !binding.armed
            || binding.binding != receipt.request.binding
            || receipt.execution_source.as_deref() != Some(source)
            || receipt.disposition != TeamWorkDisposition::Reserved
            || binding
                .grants
                .iter()
                .any(|grant| now >= grant.expires_at_ms)
        {
            return Err(work_error(
                "Standing source or shared allowance is no longer authorized",
            ));
        }
        self.verify_work_source(&binding.source)
    }

    pub(super) fn apply_standing_work_allocation(
        &self,
        controller: &crate::session_dispatch::SessionDispatchController,
        source: &str,
        repository: &axocoatl_session::turn_contract::EvidenceRef,
    ) -> Result<(), DaemonError> {
        let snapshot = controller.snapshot().map_err(work_error)?;
        let session = snapshot.owner().session_id.as_str();
        let turn = snapshot.turn_id().as_str();
        let allocations = {
            let inbox = self.work_inbox()?;
            let Some(receipt) = inbox
                .receipts()
                .map_err(work_error)?
                .iter()
                .find(|receipt| {
                    receipt.request.binding.session_id == session && receipt.turn_id == turn
                })
            else {
                return Ok(());
            };
            if receipt.execution_source.as_deref() != Some(source) {
                return Err(work_error(
                    "Standing work source differs from its exact reservation",
                ));
            }
            inbox
                .native_allocations(&receipt.receipt_id)
                .map_err(work_error)?
        };
        self.validate_standing_work_admission(session, turn, source)?;
        controller
            .install_team_work_allocations(&allocations, repository)
            .map_err(work_error)?;
        Ok(())
    }
}

fn verify_signature(secret: &[u8], body: &[u8], signature: &str) -> Result<(), DaemonError> {
    let hex = signature
        .strip_prefix("sha256=")
        .filter(|hex| hex.len() == 64 && hex.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .ok_or_else(|| work_error("Invalid producer signature"))?;
    let mut bytes = [0u8; 32];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16)
            .map_err(|_| work_error("Invalid producer signature"))?;
    }
    let mut mac = Hmac::<Sha256>::new_from_slice(secret).map_err(work_error)?;
    mac.update(body);
    mac.verify_slice(&bytes)
        .map_err(|_| work_error("Producer signature verification failed"))
}

/// The current turn must use its already-held authority. Historical/recovered
/// turns reopen only their actual existing journals; missing evidence stays an
/// unresolved allocation rather than an invented zero-usage settlement.
pub(super) fn settle_work_allocations(
    canonical: &axocoatl_session::execution_store::SessionExecutionStore,
    held: Option<(&LogicalTurnId, &ControlAuthority)>,
    id: &LogicalTurnId,
    allocations: &[axocoatl_session::team_work::DurableTeamWorkAllocation],
    at_ceiling: bool,
) -> Result<Option<Vec<axocoatl_session::team_work::TeamWorkGrantSettlement>>, DaemonError> {
    if canonical.turn(id).map_err(work_error)?.is_none() {
        return Ok(None);
    }
    let snapshot = canonical.snapshot(id).map_err(work_error)?;
    if !snapshot
        .contract()
        .state()
        .is_some_and(LogicalTurnState::is_closed)
    {
        return Ok(None);
    }
    let mut results = Vec::new();
    for allocation in allocations {
        let settled = if let Some((_, authority)) = held.filter(|(current, _)| *current == id) {
            authority.settle_team_work_allocation(&snapshot, allocation, at_ceiling)
        } else {
            let namespace = canonical
                .existing_component_namespace(
                    ExecutionComponent::ControlAuthority {
                        turn_id: id.clone(),
                    },
                    Path::new("control-authority.v1.json"),
                )
                .map_err(work_error)?;
            ControlAuthority::read_team_work_settlement_owned(
                namespace, &snapshot, allocation, at_ceiling,
            )
        };
        match settled {
            Ok(settled) => results.push(settled),
            Err(_) => return Ok(None),
        }
    }
    Ok(Some(results))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn producer_authentication_uses_exact_body_and_rejects_malformed_unicode() {
        let mut mac = Hmac::<Sha256>::new_from_slice(b"actual configured secret").unwrap();
        mac.update(b"{\"event_id\":\"one\"}");
        let signature = format!("sha256={:x}", mac.finalize().into_bytes());
        verify_signature(
            b"actual configured secret",
            b"{\"event_id\":\"one\"}",
            &signature,
        )
        .unwrap();
        assert!(verify_signature(
            b"actual configured secret",
            b"{ \"event_id\":\"one\"}",
            &signature
        )
        .is_err());
        assert!(
            verify_signature(b"another secret", b"{\"event_id\":\"one\"}", &signature).is_err()
        );
        assert!(verify_signature(b"secret", b"{}", &format!("sha256={}", "é".repeat(32))).is_err());
    }
}
