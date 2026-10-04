//! Authenticated review of exact delegated grant revisions and retained model proposals.
use super::*;
use axocoatl_session::control_authority::{AuthorityGrantStatus, GrantLimits};
use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionGrantChange {
    pub request_id: String,
    pub activation: ActivationRef,
    pub grant_id: String,
    pub expected_grant_revision: u64,
    pub limits: GrantLimits,
    pub expires_at_ms: u64,
    pub operations: Vec<DelegatedOperation>,
    pub max_nodes: u32,
    pub max_edges: u32,
    pub reason: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionGrantDecision {
    pub request: SessionGrantChange,
    pub review_digest: String,
    pub approve: bool,
    pub reason: String,
}
#[derive(Serialize)]
pub struct SessionGrantPreview {
    pub request: SessionGrantChange,
    pub before: AuthorityGrant,
    pub after: AuthorityGrant,
    pub review_digest: String,
}
#[derive(Serialize)]
pub struct SessionGrantProposal {
    pub blocker_id: BlockerId,
    pub request: SessionGrantChange,
    pub state: TurnBlockerState,
}
#[derive(Serialize)]
pub struct SessionGrantView {
    pub grants: Vec<AuthorityGrantStatus>,
    pub proposals: Vec<SessionGrantProposal>,
}
/// A grant expansion an Agent proposed through the former model control tool.
/// Nothing writes new ones; retained waits stay reviewable.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Proposal {
    kind: String,
    request: SessionGrantChange,
    grant: GrantSnapshotRef,
}
fn retained_proposal(
    content: &ExecutionContentStore,
    item: &ContractBlocker,
) -> Result<Option<SessionGrantChange>> {
    let ActivationEvidenceContent::Guidance { text } = &content
        .resolve_activation_evidence(&item.blocker.parameters)
        .map_err(error)?
    else {
        return Ok(None);
    };
    let Ok(proposal) = serde_json::from_str::<Proposal>(text) else {
        return Ok(None);
    };
    if proposal.kind != "delegated_grant_expansion_v1" {
        return Ok(None);
    }
    if proposal.request.activation != item.blocker.activation
        || item.blocker.parameters != item.blocker.safe_boundary
        || item.blocker.parameters != item.blocker.evidence
        || !matches!(&item.blocker.kind,TurnBlockerKind::HumanApproval{approval_request}if approval_request==&item.blocker.parameters)
        || item.blocker.grant.as_ref().is_none_or(|grant| {
            grant.grant_id.as_str() != proposal.request.grant_id
                || grant.revision > proposal.request.expected_grant_revision
        })
    {
        return Err(error("grant proposal differs from its exact typed wait"));
    }
    let ActivationEvidenceContent::Grant { policy } = &content
        .resolve_activation_evidence(&proposal.grant.evidence)
        .map_err(error)?
    else {
        return Err(error("grant proposal has no retained live authority"));
    };
    if proposal.grant.grant_id.as_str() != proposal.request.grant_id
        || proposal.grant.revision != proposal.request.expected_grant_revision
        || policy.id != proposal.request.grant_id
        || policy.revision != proposal.request.expected_grant_revision
        || policy.holder != proposal.request.activation.node_id
    {
        return Err(error(
            "grant proposal differs from its captured live authority",
        ));
    }
    Ok(Some(proposal.request))
}
fn same_proposal_identity(left: &SessionGrantChange, right: &SessionGrantChange) -> bool {
    left.request_id == right.request_id
        && left.activation == right.activation
        && left.grant_id == right.grant_id
        && left.expected_grant_revision == right.expected_grant_revision
}
pub(super) fn is_grant_proposal(
    content: &ExecutionContentStore,
    item: &ContractBlocker,
) -> Result<bool> {
    Ok(retained_proposal(content, item)?.is_some())
}
impl DispatchState {
    fn grant_preview(&self, request: &SessionGrantChange) -> Result<SessionGrantPreview> {
        CommandId::new(&request.request_id).map_err(error)?;
        if request.activation.session_id != self.canonical.owner().session_id
            || request.activation.turn_id != self.turn_id
            || request.reason.trim().is_empty()
            || request.reason.len() > 4096
        {
            return Err(error(
                "grant change needs an exact owner and bounded reason",
            ));
        }
        let snapshot = self.current(&request.activation)?;
        let activation = snapshot
            .contract()
            .activations()
            .iter()
            .find(|item| item.activation == request.activation)
            .ok_or_else(|| error("grant activation is absent"))?;
        if activation
            .input
            .grant
            .as_ref()
            .is_none_or(|grant| grant.grant_id.as_str() != request.grant_id)
        {
            return Err(error("grant belongs to another activation"));
        }
        let status = self
            .authority
            .grant_status(&request.grant_id)
            .map_err(error)?;
        if status.revoked_at_revision.is_some() {
            return Err(error("revoked authority cannot be expanded"));
        }
        let before = status.policy;
        if before.revision != request.expected_grant_revision
            || before.holder != request.activation.node_id
        {
            return Err(error("grant revision or holder changed"));
        }
        let mut after = before.clone();
        after.revision = after
            .revision
            .checked_add(1)
            .ok_or_else(|| error("grant revision overflow"))?;
        after.limits = request.limits.clone();
        after.expires_at_ms = request.expires_at_ms;
        let delegation = after
            .delegation
            .as_mut()
            .ok_or_else(|| error("grant has no delegated scope"))?;
        let original = delegation.operations.clone();
        delegation.operations = request
            .operations
            .iter()
            .map(|operation| {
                original
                    .iter()
                    .find(|permission| permission.operation == *operation)
                    .cloned()
                    .unwrap_or(DelegatedOperationPermission {
                        operation: *operation,
                        targets: DelegatedTargetScope::Subtree {
                            root: after.holder.clone(),
                            include_future_descendants: true,
                        },
                    })
            })
            .collect();
        delegation.graph_limits.max_nodes = request.max_nodes;
        delegation.graph_limits.max_edges = request.max_edges;
        let preserves_operations = original
            .iter()
            .all(|item| delegation.operations.contains(item));
        after.validate().map_err(error)?;
        if request.max_nodes
            < before
                .delegation
                .as_ref()
                .map(|policy| policy.graph_limits.max_nodes)
                .unwrap_or(0)
            || request.max_edges
                < before
                    .delegation
                    .as_ref()
                    .map(|policy| policy.graph_limits.max_edges)
                    .unwrap_or(0)
            || request.expires_at_ms <= now_ms()?
            || request.limits.activations < before.limits.activations
            || request.limits.invocations < before.limits.invocations
            || request.limits.tokens < before.limits.tokens
            || request.limits.cost_microunits < before.limits.cost_microunits
            || request.expires_at_ms < before.expires_at_ms
            || !preserves_operations
        {
            return Err(error("expansion must preserve existing limits and operations; use Revoke to remove authority"));
        }
        let digest = format!(
            "{:x}",
            Sha256::digest(
                serde_json::to_vec(&(snapshot.journal_id(), request, &before, &after))
                    .map_err(error)?
            )
        );
        Ok(SessionGrantPreview {
            request: request.clone(),
            before,
            after,
            review_digest: digest,
        })
    }
}
impl SessionDispatchController {
    pub(crate) fn with_grant_stores<T>(
        &self,
        use_stores: impl FnOnce(
            &SessionExecutionStore,
            &mut ExecutionContentStore,
            Option<(&LogicalTurnId, &ControlAuthority)>,
        ) -> std::result::Result<T, crate::error::DaemonError>,
    ) -> std::result::Result<T, crate::error::DaemonError> {
        let failure = |error: SessionDispatchError| {
            crate::error::DaemonError::SessionConflict(error.to_string())
        };
        let mut state = self.lock().map_err(failure)?;
        state.ready().map_err(failure)?;
        state
            .content
            .verify_canonical_owner(&state.canonical)
            .map_err(|error| crate::error::DaemonError::SessionConflict(error.to_string()))?;
        let DispatchState {
            canonical,
            content,
            turn_id,
            authority,
            ..
        } = &mut *state;
        use_stores(canonical, content, Some((turn_id, authority)))
    }
    pub(crate) fn session_grants(&self) -> Result<SessionGrantView> {
        let state = self.lock()?;
        let snapshot = state.canonical.snapshot(&state.turn_id).map_err(error)?;
        let mut ids = HashSet::new();
        let mut grants = vec![];
        for activation in snapshot.contract().activations() {
            if let Some(grant) = &activation.input.grant {
                if ids.insert(grant.grant_id.clone()) {
                    grants.push(
                        state
                            .authority
                            .grant_status(grant.grant_id.as_str())
                            .map_err(error)?,
                    );
                }
            }
        }
        let mut proposals = vec![];
        for item in snapshot.contract().blockers() {
            if let Some(request) = retained_proposal(&state.content, item)? {
                proposals.push(SessionGrantProposal {
                    blocker_id: item.blocker.blocker_id.clone(),
                    request,
                    state: item.state.clone(),
                });
            }
        }
        Ok(SessionGrantView { grants, proposals })
    }
    pub(crate) fn preview_grant_change(
        &self,
        request: &SessionGrantChange,
    ) -> Result<SessionGrantPreview> {
        self.lock()?.grant_preview(request)
    }
    pub(crate) fn decide_grant_change(
        &self,
        decision: &SessionGrantDecision,
    ) -> Result<SessionGrantView> {
        let mut state = self.lock()?;
        let snapshot = state.canonical.snapshot(&state.turn_id).map_err(error)?;
        let mut blocker = None;
        for item in snapshot.contract().blockers() {
            if retained_proposal(&state.content, item)?
                .is_some_and(|request| same_proposal_identity(&request, &decision.request))
            {
                blocker = Some(item.clone());
                break;
            }
        }
        let text = serde_json::to_string(
            &serde_json::json!({"kind":"authenticated_grant_decision_v1","decision":decision}),
        )
        .map_err(error)?;
        let evidence = state
            .content
            .retain_activation_evidence(ActivationEvidenceContent::Guidance { text })
            .map_err(error)?
            .reference()
            .clone();
        if let Some(item) = &blocker {
            if let TurnBlockerState::Resolved { response } = &item.state {
                let reference = match response {
                    TurnBlockerResponse::HumanApproval {
                        approval_evidence, ..
                    } => approval_evidence,
                    TurnBlockerResponse::HumanDecline { reason, .. } => reason,
                    _ => return Err(error("grant proposal has another response")),
                };
                if reference != &evidence {
                    return Err(error(
                        "grant proposal already has a different human decision",
                    ));
                }
                drop(state);
                return self.session_grants();
            }
        }
        if decision.approve {
            if state
                .bound
                .get(&decision.request.activation.activation_id)
                .is_none_or(|bound| {
                    bound.activation != decision.request.activation || bound.control.is_cancelled()
                })
            {
                return Err(error("grant expansion requires its exact live holder; a lost actor cannot be resumed by approval"));
            }
            let next_revision = decision
                .request
                .expected_grant_revision
                .checked_add(1)
                .ok_or_else(|| error("grant revision overflow"))?;
            let after = if state
                .authority
                .has_expansion_approval(&decision.request.grant_id, next_revision, &evidence)
                .map_err(error)?
            {
                state
                    .authority
                    .recorded_grant_policy(&decision.request.grant_id, next_revision)
                    .map_err(error)?
            } else {
                let preview = state.grant_preview(&decision.request)?;
                if preview.review_digest != decision.review_digest {
                    return Err(error("grant review is stale or changed"));
                }
                let revision = state.authority.revision().map_err(error)?;
                state
                    .authority
                    .approve_expanded_grant(preview.after.clone(), evidence.clone(), revision)
                    .map_err(error)?;
                preview.after
            };
            let grant = GrantSnapshotRef {
                grant_id: GrantId::new(&after.id).map_err(error)?,
                revision: after.revision,
                evidence: state
                    .content
                    .retain_activation_evidence(ActivationEvidenceContent::Grant { policy: after })
                    .map_err(error)?
                    .reference()
                    .clone(),
            };
            let ids = state
                .bound
                .iter()
                .filter(|(_, bound)| bound.grant.grant_id == grant.grant_id)
                .map(|(id, _)| id.clone())
                .collect::<Vec<_>>();
            for id in ids {
                let lease = state
                    .authority
                    .renew_expanded_lease(&state.bound[&id].lease, now_ms()?)
                    .map_err(error)?;
                if let Some(bound) = state.bound.get_mut(&id) {
                    bound.lease = lease;
                    bound.grant = grant.clone();
                }
            }
        } else if decision.reason.trim().is_empty() {
            return Err(error("Decline requires a reason"));
        }
        if let Some(item) = blocker {
            if item.state != TurnBlockerState::Pending {
                return Err(error("grant proposal is no longer pending"));
            }
            let response = if decision.approve {
                TurnBlockerResponse::HumanApproval {
                    approval_request: item.blocker.parameters.clone(),
                    approval_evidence: evidence,
                }
            } else {
                TurnBlockerResponse::HumanDecline {
                    approval_request: item.blocker.parameters.clone(),
                    reason: evidence,
                }
            };
            state.append(
                &format!("grant-decision-{}", decision.request.request_id),
                TurnContractEvent::ResolveBlocker {
                    blocker_id: item.blocker.blocker_id,
                    activation: item.blocker.activation,
                    response,
                },
            )?;
        }
        state.changed.notify_waiters();
        drop(state);
        self.session_grants()
    }
    pub(crate) fn revoke_control_grant(&self, id: &str, expected: u64) -> Result<SessionGrantView> {
        let mut state = self.lock()?;
        let status = state.authority.grant_status(id).map_err(error)?;
        if status.policy.revision != expected {
            return Err(error("grant revision changed"));
        }
        let revision = state.authority.revision().map_err(error)?;
        state.authority.revoke_grant(id, revision).map_err(error)?;
        state.reconcile_control_commands()?;
        state.changed.notify_waiters();
        drop(state);
        self.session_grants()
    }
}
pub(crate) fn retained_grant_view(
    canonical: &SessionExecutionStore,
    content: &ExecutionContentStore,
    turn: &LogicalTurnId,
    held: Option<(&LogicalTurnId, &ControlAuthority)>,
) -> Result<SessionGrantView> {
    let snapshot = canonical.snapshot(turn).map_err(error)?;
    let grants = if let Some((_, authority)) = held.filter(|(current, _)| *current == turn) {
        authority.grant_statuses().map_err(error)?
    } else {
        let namespace = canonical
            .existing_component_namespace(
                ExecutionComponent::ControlAuthority {
                    turn_id: turn.clone(),
                },
                std::path::Path::new("control-authority.v1.json"),
            )
            .map_err(error)?;
        ControlAuthority::read_grant_statuses_owned(&namespace).map_err(error)?
    };
    let mut proposals = vec![];
    for item in snapshot.contract().blockers() {
        if let Some(request) = retained_proposal(content, item)? {
            proposals.push(SessionGrantProposal {
                blocker_id: item.blocker.blocker_id.clone(),
                request,
                state: item.state.clone(),
            });
        }
    }
    Ok(SessionGrantView { grants, proposals })
}
pub(crate) fn retained_grant_decision(
    canonical: &SessionExecutionStore,
    content: &mut ExecutionContentStore,
    turn: &LogicalTurnId,
    decision: &SessionGrantDecision,
    held: Option<(&LogicalTurnId, &ControlAuthority)>,
) -> Result<Option<SessionGrantView>> {
    if decision.request.activation.session_id != canonical.owner().session_id
        || &decision.request.activation.turn_id != turn
    {
        return Err(error("grant decision belongs to another exact owner"));
    }
    let evidence = content
        .retain_activation_evidence(ActivationEvidenceContent::Guidance {
            text: serde_json::to_string(
                &serde_json::json!({"kind":"authenticated_grant_decision_v1","decision":decision}),
            )
            .map_err(error)?,
        })
        .map_err(error)?
        .reference()
        .clone();
    let snapshot = canonical.snapshot(turn).map_err(error)?;
    for item in snapshot.contract().blockers() {
        if retained_proposal(content, item)?
            .is_some_and(|request| same_proposal_identity(&request, &decision.request))
        {
            if let TurnBlockerState::Resolved { response } = &item.state {
                let recorded = match response {
                    TurnBlockerResponse::HumanApproval {
                        approval_evidence, ..
                    } => approval_evidence,
                    TurnBlockerResponse::HumanDecline { reason, .. } => reason,
                    _ => return Err(error("grant proposal has another response")),
                };
                if recorded != &evidence {
                    return Err(error("grant proposal already has a different decision"));
                }
                return retained_grant_view(canonical, content, turn, held).map(Some);
            }
            return Ok(None);
        }
    }
    if decision.approve {
        let next_revision = decision
            .request
            .expected_grant_revision
            .checked_add(1)
            .ok_or_else(|| error("grant revision overflow"))?;
        let recorded = if let Some((_, authority)) = held.filter(|(current, _)| *current == turn) {
            authority
                .has_expansion_approval(&decision.request.grant_id, next_revision, &evidence)
                .map_err(error)?
        } else {
            let namespace = canonical
                .existing_component_namespace(
                    ExecutionComponent::ControlAuthority {
                        turn_id: turn.clone(),
                    },
                    std::path::Path::new("control-authority.v1.json"),
                )
                .map_err(error)?;
            ControlAuthority::read_expansion_approval_owned(
                &namespace,
                &decision.request.grant_id,
                next_revision,
                &evidence,
            )
            .map_err(error)?
        };
        if recorded {
            return retained_grant_view(canonical, content, turn, held).map(Some);
        }
    }
    Ok(None)
}
