//! Carried standing-team allowance is installed from an acknowledged inbox.
use super::*;
use crate::team_work::budget::{Settlement, SettlementBasis};
use crate::team_work::{
    DurableTeamWorkAllocation, TeamWorkGrantReference, TeamWorkGrantSettlement,
};

#[cfg(test)]
#[path = "control_authority_standing_tests.rs"]
mod tests;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct StandingGrantCarry {
    receipt_id: String,
    grant: TeamWorkGrantReference,
    consumed_before: GrantUsage,
    #[serde(default)]
    pub(super) conditions: Vec<ConditionPermission>,
}

impl ControlAuthority {
    /// No caller-supplied usage is accepted: the capability comes from the
    /// durable inbox reservation and names this exact Session and turn.
    pub fn apply_team_work_allocation(
        &self,
        allocation: &DurableTeamWorkAllocation,
    ) -> Result<(), AuthorityError> {
        let mut state = self.lock()?;
        let index = grant_index(&state.data, allocation.grant_id())?;
        let grant = &state.data.grants[index];
        if !allocation.verify(
            state.data.session_id.as_str(),
            state.data.turn_id.as_str(),
            &grant.policy,
        ) {
            return Err(AuthorityError::Denied);
        }
        let carry = StandingGrantCarry {
            receipt_id: allocation.receipt_id.clone(),
            grant: allocation.allocation.grant.clone(),
            consumed_before: allocation.allocation.consumed_before.clone(),
            conditions: Vec::new(),
        };
        if let Some(existing) = &grant.standing {
            return if existing.receipt_id == carry.receipt_id
                && existing.grant == carry.grant
                && existing.consumed_before == carry.consumed_before
            {
                Ok(())
            } else {
                Err(AuthorityError::Denied)
            };
        }
        if state.data.closed
            || grant.usage != GrantUsage::default()
            || grant.delegated_from.is_some()
            || state
                .data
                .activations
                .iter()
                .any(|activation| activation.grant_id == grant.policy.id)
        {
            return Err(AuthorityError::Denied);
        }
        let mut next = state.data.clone();
        next.grants[index].usage = carry.consumed_before.clone();
        next.grants[index].standing = Some(carry);
        self.commit(&mut state, next)
    }

    /// Human-armed check definitions grant only the independent host condition
    /// port. They do not widen an Agent's tools, graph commands, or allowance.
    pub fn authorize_team_work_conditions(
        &self,
        allocation: &DurableTeamWorkAllocation,
        snapshot: &DurableTurnSnapshot,
        content: &ExecutionContentStore,
        repository: &EvidenceRef,
        isolation: &str,
    ) -> Result<(), AuthorityError> {
        let mut state = self.lock()?;
        let index = grant_index(&state.data, allocation.grant_id())?;
        let record = &state.data.grants[index];
        let carry = record.standing.as_ref().ok_or(AuthorityError::Denied)?;
        if carry.receipt_id != allocation.receipt_id
            || !allocation.verify(
                state.data.session_id.as_str(),
                state.data.turn_id.as_str(),
                &record.policy,
            )
            || snapshot.journal_id()
                != state
                    .data
                    .canonical_journal_id
                    .as_deref()
                    .ok_or(AuthorityError::Denied)?
            || snapshot.turn_id() != &state.data.turn_id
            || record.revoked_at_revision.is_some()
        {
            return Err(AuthorityError::Denied);
        }
        if !record
            .policy
            .profiles
            .iter()
            .any(|profile| profile.tools.iter().any(|tool| tool == "bash"))
        {
            return Ok(());
        }
        let graph = snapshot.contract().graph().ok_or(AuthorityError::Denied)?;
        let nodes: Vec<_> = graph
            .nodes
            .iter()
            .filter(|node| node.required)
            .map(|node| node.node_id.clone())
            .collect();
        let definitions = crate::team_work::standing_check_definitions(&allocation.required_checks)
            .map_err(|_| AuthorityError::Denied)?;
        let mut permissions = Vec::new();
        for (ordinal, definition) in definitions.iter().enumerate() {
            let reference = content
                .retained_check_definition(definition)
                .map_err(|_| AuthorityError::Denied)?
                .ok_or(AuthorityError::Denied)?;
            let kind = ConditionKind::RepositoryCheck {
                definition: reference,
            };
            let id = crate::team_work::standing_condition_id(&allocation.receipt_id, ordinal);
            if !graph.conditions.iter().any(|condition| {
                condition.condition_id.as_str() == id
                    && condition.kind == kind
                    && condition.nodes == nodes
            }) {
                return Err(AuthorityError::Denied);
            }
            let permission = ConditionPermission {
                kind,
                nodes: nodes.clone(),
                repository: repository.clone(),
                isolation: isolation.into(),
                max_timeout_ms: definition.timeout_ms,
                max_stdout_bytes: definition.stdout_bytes,
                max_stderr_bytes: definition.stderr_bytes,
            };
            if !permissions.contains(&permission) {
                permissions.push(permission);
            }
        }
        if carry.conditions == permissions {
            return Ok(());
        }
        if !carry.conditions.is_empty() {
            return Err(AuthorityError::Denied);
        }
        let mut next = state.data.clone();
        next.grants[index]
            .standing
            .as_mut()
            .ok_or(AuthorityError::Denied)?
            .conditions = permissions;
        self.commit(&mut state, next)
    }

    /// Whether this grant carries standing check permissions, and so may be
    /// charged for the host's required-check runs. Delegated grants never are.
    pub fn grant_pays_standing_checks(&self, grant_id: &str) -> Result<bool, AuthorityError> {
        let state = self.lock()?;
        Ok(state.data.grants[grant_index(&state.data, grant_id)?]
            .standing
            .as_ref()
            .is_some_and(|carry| !carry.conditions.is_empty()))
    }

    pub fn team_work_condition_grant(
        &self,
        definition: &EvidenceRef,
        repository: &EvidenceRef,
        now_ms: u64,
    ) -> Result<Option<String>, AuthorityError> {
        let state = self.lock()?;
        Ok(state
            .data
            .grants
            .iter()
            .find(|record| {
                record.revoked_at_revision.is_none()
                    && record
                        .policy
                        .profiles
                        .iter()
                        .any(|profile| profile.tools.iter().any(|tool| tool == "bash"))
                    && !state.data.closed
                    && now_ms < record.policy.expires_at_ms
                    && record.usage.invocations < record.policy.limits.invocations
                    && record.standing.as_ref().is_some_and(|carry| {
                        carry.conditions.iter().any(|permission| {
                            permission.repository == *repository
                                && permission.kind
                                    == (ConditionKind::RepositoryCheck {
                                        definition: definition.clone(),
                                    })
                        })
                    })
            })
            .map(|record| record.policy.id.clone()))
    }

    /// Release shared capacity only from actual complete accounting of a closed
    /// canonical turn. Missing outcomes retain the entire remaining allocation,
    /// unless a person chose `at_ceiling`: then each provider call whose usage
    /// stayed unknown is charged its full admitted reservation. Unknown tool
    /// effects and check outcomes still refuse.
    pub fn settle_team_work_allocation(
        &self,
        snapshot: &DurableTurnSnapshot,
        allocation: &DurableTeamWorkAllocation,
        at_ceiling: bool,
    ) -> Result<TeamWorkGrantSettlement, AuthorityError> {
        let state = self.lock()?;
        settle(&state.data, snapshot, allocation, at_ceiling)
    }

    pub fn read_team_work_settlement_owned(
        namespace: OwnedExecutionNamespace,
        snapshot: &DurableTurnSnapshot,
        allocation: &DurableTeamWorkAllocation,
        at_ceiling: bool,
    ) -> Result<TeamWorkGrantSettlement, AuthorityError> {
        namespace.require_root(&ExecutionComponent::ControlAuthority {
            turn_id: snapshot.turn_id().clone(),
        })?;
        if namespace.identity().owner() != snapshot.owner()
            || namespace.identity().journal_id() != snapshot.journal_id()
        {
            return Err(AuthorityError::Denied);
        }
        let bytes = namespace.read_limited(FILE, MAX_BYTES)?;
        let data: AuthorityData = serde_json::from_slice(&bytes)?;
        validate_data(&data)?;
        let receipt = settle(&data, snapshot, allocation, at_ceiling)?;
        namespace
            .secure_dir()?
            .open_file_limited(FILE, MAX_BYTES)?
            .sync_all()?;
        namespace.sync_all()?;
        if namespace.read_limited(FILE, MAX_BYTES)? != bytes {
            return Err(AuthorityError::RecoveryRequired);
        }
        Ok(receipt)
    }
}

pub(super) fn condition_allowed(
    record: &ConditionCallRecord,
    grant: &GrantRecord,
    policy: &AuthorityGrant,
) -> bool {
    checks::has_shell(policy)
        && grant.standing.as_ref().is_some_and(|carry| {
            carry
                .conditions
                .iter()
                .any(|permission| checks::permission_covers(permission, record))
        })
}

/// A standing check's permission derived through canonical replacement; see
/// [`checks::replaced_permission`].
pub(super) fn replaced_condition_permission(
    snapshot: &DurableTurnSnapshot,
    record: &ConditionCallRecord,
    grant: &GrantRecord,
) -> Option<ConditionPermission> {
    checks::replaced_permission(
        snapshot,
        record,
        &grant.standing.as_ref()?.conditions,
        &grant.policy,
    )
}

pub(super) fn validate_carry(
    data: &AuthorityData,
    grant: &GrantRecord,
) -> Result<GrantUsage, AuthorityError> {
    let Some(carry) = &grant.standing else {
        return Ok(GrantUsage::default());
    };
    bounded(&carry.receipt_id, 512)?;
    let original = grant.previous_policies.first().unwrap_or(&grant.policy);
    if carry.grant.id != original.id
        || carry.grant.revision != original.revision
        || carry.grant.limits != original.limits
        || carry.grant.expires_at_ms != original.expires_at_ms
        || grant.delegated_from.is_some()
        || data.canonical_journal_id.is_none()
        || carry.consumed_before.activations > original.limits.activations
        || carry.consumed_before.invocations > original.limits.invocations
        || carry.consumed_before.tokens > original.limits.tokens
        || carry.consumed_before.cost_microunits > original.limits.cost_microunits
    {
        return Err(AuthorityError::Invalid(
            "standing grant carry differs from approved allowance",
        ));
    }
    checks::validate_permissions(
        &carry.conditions,
        original,
        "invalid standing condition permission",
    )?;
    Ok(carry.consumed_before.clone())
}

pub(super) fn settle(
    data: &AuthorityData,
    snapshot: &DurableTurnSnapshot,
    allocation: &DurableTeamWorkAllocation,
    at_ceiling: bool,
) -> Result<TeamWorkGrantSettlement, AuthorityError> {
    if data.canonical_journal_id.as_deref() != Some(snapshot.journal_id())
        || snapshot.owner().session_id != data.session_id
        || snapshot.turn_id() != &data.turn_id
        || !snapshot
            .contract()
            .state()
            .is_some_and(LogicalTurnState::is_closed)
    {
        return Err(AuthorityError::Denied);
    }
    let root = &data.grants[grant_index(data, allocation.grant_id())?];
    let original = root.previous_policies.first().unwrap_or(&root.policy);
    if !allocation.verify(data.session_id.as_str(), data.turn_id.as_str(), original) {
        return Err(AuthorityError::Denied);
    }
    let carry = root.standing.as_ref().ok_or(AuthorityError::Denied)?;
    if carry.receipt_id != allocation.receipt_id
        || carry.consumed_before != allocation.allocation.consumed_before
    {
        return Err(AuthorityError::Denied);
    }
    let mut ids = HashSet::from([root.policy.id.as_str()]);
    loop {
        let previous = ids.len();
        for grant in &data.grants {
            if grant
                .delegated_from
                .as_ref()
                .is_some_and(|parent| ids.contains(parent.parent_grant_id.as_str()))
            {
                ids.insert(grant.policy.id.as_str());
            }
        }
        if previous == ids.len() {
            break;
        }
    }
    let mut used = carry.consumed_before.clone();
    for activation in data
        .activations
        .iter()
        .filter(|item| ids.contains(item.grant_id.as_str()) && !item.never_dispatched)
    {
        // A canonical closed turn still requires real effect settlement below.
        let _ = activation;
        used.activations = used
            .activations
            .checked_add(1)
            .ok_or(AuthorityError::Capacity)?;
    }
    for claim in data
        .claims
        .iter()
        .filter(|item| ids.contains(item.grant_id.as_str()))
    {
        if !claim.settled || claim.reservation.tokens != 0 || claim.reservation.cost_microunits != 0
        {
            return Err(AuthorityError::Denied);
        }
        used.invocations = used
            .invocations
            .checked_add(1)
            .ok_or(AuthorityError::Capacity)?;
    }
    let mut unknown_calls = 0u32;
    for call in data
        .provider_calls
        .iter()
        .filter(|item| ids.contains(item.grant_id.as_str()))
    {
        if provider_bound_violation(call) {
            return Err(AuthorityError::Denied);
        }
        let measured = call
            .outcome
            .as_ref()
            .filter(|outcome| outcome.usage.complete);
        let (tokens, cost) = match measured {
            Some(outcome) => {
                let cost = if call.intent.reservation.cost_microunits == 0 {
                    0
                } else if outcome.cost_known {
                    outcome.cost_microunits.ok_or(AuthorityError::Denied)?
                } else {
                    return Err(AuthorityError::Denied);
                };
                let tokens = &outcome.usage.usage;
                let tokens = tokens
                    .input_tokens
                    .checked_add(tokens.output_tokens)
                    .and_then(|n| n.checked_add(tokens.reasoning_tokens.unwrap_or(0)))
                    .and_then(|n| u64::try_from(n).ok())
                    .ok_or(AuthorityError::Capacity)?;
                (tokens, cost)
            }
            // The admitted reservation bounds what the call could consume; a
            // provider cannot exceed it without a recorded bound violation.
            None if at_ceiling => {
                unknown_calls = unknown_calls
                    .checked_add(1)
                    .ok_or(AuthorityError::Capacity)?;
                (
                    call.intent.reservation.tokens,
                    call.intent.reservation.cost_microunits,
                )
            }
            None => return Err(AuthorityError::Denied),
        };
        used.invocations = used
            .invocations
            .checked_add(1)
            .ok_or(AuthorityError::Capacity)?;
        used.tokens = used
            .tokens
            .checked_add(tokens)
            .ok_or(AuthorityError::Capacity)?;
        used.cost_microunits = used
            .cost_microunits
            .checked_add(cost)
            .ok_or(AuthorityError::Capacity)?;
    }
    for call in data
        .condition_calls
        .iter()
        .filter(|item| ids.contains(item.grant.grant_id.as_str()))
    {
        if call.result.is_none() {
            return Err(AuthorityError::Denied);
        }
        used.invocations = used
            .invocations
            .checked_add(1)
            .ok_or(AuthorityError::Capacity)?;
    }
    Ok(TeamWorkGrantSettlement {
        receipt_id: allocation.receipt_id.clone(),
        session_id: data.session_id.as_str().into(),
        turn_id: data.turn_id.as_str().into(),
        grant: allocation.allocation.grant.clone(),
        consumed_before: carry.consumed_before.clone(),
        settled: Settlement {
            total_consumed: used,
            revoked: root.revoked_at_revision.is_some() || root.policy != *original,
            basis: if unknown_calls == 0 {
                SettlementBasis::Measured
            } else {
                SettlementBasis::ReservedCeiling { unknown_calls }
            },
        },
    })
}
