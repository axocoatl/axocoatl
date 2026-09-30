//! Explicit host-approved expansion. Accumulated reservations never reset.
use super::*;
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ApprovedExpansion {
    pub revision: u64,
    pub evidence: EvidenceRef,
}
impl ControlAuthority {
    /// Privileged authenticated-host operation. Callers retain the complete
    /// human decision and exact old/new policy before supplying its evidence.
    pub fn approve_expanded_grant(
        &self,
        grant: AuthorityGrant,
        approval: EvidenceRef,
        expected_revision: u64,
    ) -> Result<(), AuthorityError> {
        validate_grant(&grant)?;
        let mut state = self.lock()?;
        let index = grant_index(&state.data, &grant.id)?;
        let old = &state.data.grants[index];
        if old.policy == grant
            && old
                .expansions
                .iter()
                .any(|item| item.revision == grant.revision && item.evidence == approval)
        {
            return Ok(());
        }
        check_revision(&state.data, expected_revision)?;
        if state.data.closed
            || old.revoked_at_revision.is_some()
            || old.delegated_from.is_some()
            || !expanded(&grant, &old.policy)
            || old.previous_policies.len() >= 63
        {
            return Err(AuthorityError::Denied);
        }
        let mut next = state.data.clone();
        next.grants[index]
            .previous_policies
            .push(old.policy.clone());
        next.grants[index].expansions.push(ApprovedExpansion {
            revision: grant.revision,
            evidence: approval,
        });
        next.grants[index].policy = grant;
        self.commit(&mut state, next)
    }
    pub fn has_expansion_approval(
        &self,
        id: &str,
        revision: u64,
        evidence: &EvidenceRef,
    ) -> Result<bool, AuthorityError> {
        let state = self.lock()?;
        Ok(state.data.grants[grant_index(&state.data, id)?]
            .expansions
            .iter()
            .any(|item| item.revision == revision && &item.evidence == evidence))
    }
    pub fn recorded_grant_policy(
        &self,
        id: &str,
        revision: u64,
    ) -> Result<AuthorityGrant, AuthorityError> {
        let state = self.lock()?;
        let grant = &state.data.grants[grant_index(&state.data, id)?];
        grant
            .previous_policies
            .iter()
            .chain(std::iter::once(&grant.policy))
            .find(|policy| policy.revision == revision)
            .cloned()
            .ok_or(AuthorityError::Denied)
    }
    pub fn permits_captured_grant(
        &self,
        captured: &AuthorityGrant,
    ) -> Result<bool, AuthorityError> {
        let state = self.lock()?;
        let grant = &state.data.grants[grant_index(&state.data, &captured.id)?];
        Ok(grant.revoked_at_revision.is_none()
            && grant
                .previous_policies
                .iter()
                .chain(std::iter::once(&grant.policy))
                .any(|policy| policy == captured)
            && revision_path(grant, captured.revision, grant.policy.revision))
    }
    pub fn renew_expanded_lease(
        &self,
        lease: &ActivationLease,
        now_ms: u64,
    ) -> Result<ActivationLease, AuthorityError> {
        let state = self.lock()?;
        let grant = &state.data.grants[grant_index(&state.data, &lease.grant_id)?];
        if lease.scope != self.scope
            || !revision_path(grant, lease.grant_revision, grant.policy.revision)
        {
            return Err(AuthorityError::Denied);
        }
        let renewed = ActivationLease {
            activation: lease.activation.clone(),
            grant_id: lease.grant_id.clone(),
            grant_revision: grant.policy.revision,
            scope: lease.scope.clone(),
        };
        self.validate_lease(&state.data, &renewed, now_ms)?;
        Ok(renewed)
    }
}
pub(super) fn revision_path(grant: &GrantRecord, from: u64, to: u64) -> bool {
    from > 0
        && from <= to
        && (from..to).all(|revision| {
            grant
                .expansions
                .iter()
                .any(|item| item.revision == revision.saturating_add(1))
        })
}
pub(super) fn expanded(new: &AuthorityGrant, old: &AuthorityGrant) -> bool {
    if Some(new.revision) != old.revision.checked_add(1)
        || new.limits.activations < old.limits.activations
        || new.limits.invocations < old.limits.invocations
        || new.limits.tokens < old.limits.tokens
        || new.limits.cost_microunits < old.limits.cost_microunits
        || new.expires_at_ms < old.expires_at_ms
    {
        return false;
    }
    let mut equal = new.clone();
    equal.revision = old.revision;
    equal.limits = old.limits.clone();
    equal.expires_at_ms = old.expires_at_ms;
    match (equal.delegation.as_mut(), old.delegation.as_ref()) {
        (Some(next), Some(previous)) => {
            if next.graph_limits.max_nodes < previous.graph_limits.max_nodes
                || next.graph_limits.max_edges < previous.graph_limits.max_edges
                || !previous
                    .operations
                    .iter()
                    .all(|operation| next.operations.contains(operation))
            {
                return false;
            }
            next.graph_limits = previous.graph_limits.clone();
            next.operations = previous.operations.clone();
        }
        (None, None) => {}
        _ => return false,
    }
    equal == *old
}
pub(super) fn recorded_revision(data: &AuthorityData, grant_id: &str, from: u64, to: u64) -> bool {
    grant_index(data, grant_id)
        .ok()
        .is_some_and(|index| revision_path(&data.grants[index], from, to))
}
impl ControlAuthority {
    /// Read through the existing writer lease without acquiring its namespace again.
    pub fn grant_statuses(&self) -> Result<Vec<AuthorityGrantStatus>, AuthorityError> {
        let state = self.lock()?;
        Ok(grant_statuses(&state.data))
    }
    pub fn read_grant_statuses_owned(
        namespace: &OwnedExecutionNamespace,
    ) -> Result<Vec<AuthorityGrantStatus>, AuthorityError> {
        let data = read_owned(namespace)?;
        Ok(grant_statuses(&data))
    }
    pub fn read_expansion_approval_owned(
        namespace: &OwnedExecutionNamespace,
        id: &str,
        revision: u64,
        evidence: &EvidenceRef,
    ) -> Result<bool, AuthorityError> {
        let data = read_owned(namespace)?;
        Ok(data.grants[grant_index(&data, id)?]
            .expansions
            .iter()
            .any(|item| item.revision == revision && &item.evidence == evidence))
    }
}
fn grant_statuses(data: &AuthorityData) -> Vec<AuthorityGrantStatus> {
    data.grants
        .iter()
        .map(|record| AuthorityGrantStatus {
            policy: record.policy.clone(),
            authority_revision: data.revision,
            revoked_at_revision: record.revoked_at_revision,
        })
        .collect()
}
fn read_owned(namespace: &OwnedExecutionNamespace) -> Result<AuthorityData, AuthorityError> {
    let ExecutionComponent::ControlAuthority { turn_id } = namespace.component() else {
        return Err(AuthorityError::Denied);
    };
    namespace.require_root(&ExecutionComponent::ControlAuthority {
        turn_id: turn_id.clone(),
    })?;
    let data: AuthorityData = serde_json::from_slice(&namespace.read_limited(FILE, MAX_BYTES)?)?;
    validate_data(&data)?;
    if &data.turn_id != turn_id
        || data.session_id != namespace.identity().owner().session_id
        || data.canonical_journal_id.as_deref() != Some(namespace.identity().journal_id())
    {
        return Err(AuthorityError::Denied);
    }
    Ok(data)
}
