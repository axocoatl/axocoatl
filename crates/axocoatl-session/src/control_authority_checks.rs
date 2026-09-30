//! A turn's host-run required checks are paid by one Agent grant.
//!
//! Required checks are conditions of the admitted turn graph. The host runs
//! them through the independent condition port, charged to the grant of the
//! first required Agent whose own profile may use bash. That grant carries
//! permission to run exactly those checks against this turn's repository and
//! nothing else: no tool, graph command or allowance is widened.
use super::*;
use crate::turn_checks::REQUIRED_CHECK_PREFIX;
use crate::turn_contract::{GraphNode, TurnGraphSnapshot};

/// The grant that pays for a turn's required checks, and what it has left.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequiredCheckPayer {
    pub grant_id: String,
    pub holder: TurnNodeId,
    /// Invocations the grant may still spend.
    pub invocations_left: u32,
    pub revoked: bool,
    pub expires_at_ms: u64,
    /// Whether a person can raise its limits during the turn: only a lead's
    /// grant carries delegation that Current work authority can widen.
    pub delegating: bool,
    /// The turn's authority is closed; nothing more runs.
    pub closed: bool,
}

#[cfg(test)]
#[path = "control_authority_checks_tests.rs"]
mod tests;

impl ControlAuthority {
    /// Retain the permission to run the admitted graph's required checks on
    /// the grant that pays for them. It is computed from the graph admitted
    /// at Begin, so repeating it after a replacement is still idempotent;
    /// permissions derived by replacement are appended after it. A different
    /// stored set, or no grant that may pay, is refused.
    pub fn authorize_required_checks(
        &self,
        snapshot: &DurableTurnSnapshot,
        content: &ExecutionContentStore,
        repository: &EvidenceRef,
        isolation: &str,
    ) -> Result<(), AuthorityError> {
        bounded(isolation, 128)?;
        let mut state = self.lock()?;
        if snapshot.journal_id()
            != state
                .data
                .canonical_journal_id
                .as_deref()
                .ok_or(AuthorityError::Denied)?
            || snapshot.turn_id() != &state.data.turn_id
        {
            return Err(AuthorityError::Denied);
        }
        let contract = snapshot.contract();
        let graph = contract
            .graph_history()
            .first()
            .map(|revision| &revision.previous)
            .or(contract.graph())
            .ok_or(AuthorityError::Denied)?;
        let mut permissions = Vec::new();
        for condition in graph.conditions.iter().filter(|condition| {
            condition
                .condition_id
                .as_str()
                .starts_with(REQUIRED_CHECK_PREFIX)
        }) {
            let ConditionKind::RepositoryCheck { definition } = &condition.kind else {
                continue;
            };
            let definition = content
                .resolve_repository_check_definition(definition)
                .map_err(|_| AuthorityError::Denied)?;
            let permission = ConditionPermission {
                kind: condition.kind.clone(),
                nodes: condition.nodes.clone(),
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
        if permissions.is_empty() {
            return Ok(());
        }
        let index = paying_grant(&state.data, graph).ok_or(AuthorityError::Denied)?;
        if state
            .data
            .grants
            .iter()
            .enumerate()
            .any(|(other, grant)| other != index && !grant.host_checks.is_empty())
        {
            return Err(AuthorityError::Denied);
        }
        let stored = &state.data.grants[index].host_checks;
        if stored.starts_with(&permissions) {
            return Ok(());
        }
        if !stored.is_empty() || state.data.closed {
            return Err(AuthorityError::Denied);
        }
        let mut next = state.data.clone();
        next.grants[index].host_checks = permissions;
        self.commit(&mut state, next)
    }

    /// Whether this grant pays for the turn's required checks, so the host
    /// holds back their invocations from its Agent's own allowance.
    pub fn grant_pays_required_checks(&self, grant_id: &str) -> Result<bool, AuthorityError> {
        let state = self.lock()?;
        Ok(!state.data.grants[grant_index(&state.data, grant_id)?]
            .host_checks
            .is_empty())
    }

    /// The grant that pays for this turn's required checks, if one was
    /// authorized, with what it has left.
    pub fn required_check_payer(&self) -> Result<Option<RequiredCheckPayer>, AuthorityError> {
        let state = self.lock()?;
        Ok(state
            .data
            .grants
            .iter()
            .find(|record| !record.host_checks.is_empty())
            .map(|record| RequiredCheckPayer {
                grant_id: record.policy.id.clone(),
                holder: record.policy.holder.clone(),
                invocations_left: record
                    .policy
                    .limits
                    .invocations
                    .saturating_sub(record.usage.invocations),
                revoked: record.revoked_at_revision.is_some(),
                expires_at_ms: record.policy.expires_at_ms,
                delegating: record.policy.delegation.is_some(),
                closed: state.data.closed,
            }))
    }

    /// The grant that may run this exact required check now, if any.
    pub fn required_check_grant(
        &self,
        definition: &EvidenceRef,
        repository: &EvidenceRef,
        now_ms: u64,
    ) -> Result<Option<String>, AuthorityError> {
        let state = self.lock()?;
        let kind = ConditionKind::RepositoryCheck {
            definition: definition.clone(),
        };
        Ok(state
            .data
            .grants
            .iter()
            .find(|record| {
                !state.data.closed
                    && record.revoked_at_revision.is_none()
                    && record.delegated_from.is_none()
                    && has_shell(&record.policy)
                    && now_ms < record.policy.expires_at_ms
                    && record.usage.invocations < record.policy.limits.invocations
                    && record.host_checks.iter().any(|permission| {
                        permission.repository == *repository && permission.kind == kind
                    })
            })
            .map(|record| record.policy.id.clone()))
    }
}

/// The grant of the first required node, in graph order, that is installed,
/// unrevoked, not delegated, and whose node's own profile may use bash.
fn paying_grant(data: &AuthorityData, graph: &TurnGraphSnapshot) -> Option<usize> {
    graph
        .nodes
        .iter()
        .filter(|node| node.required)
        .find_map(|node| {
            data.grants.iter().position(|record| {
                record.policy.holder == node.node_id
                    && record.revoked_at_revision.is_none()
                    && record.delegated_from.is_none()
                    && pays_with_own_shell(&record.policy, node)
            })
        })
}

/// Whether `node`'s own profile in `policy` may use bash. A lead's grant also
/// carries the profiles of the helpers it may start; their tools never make
/// the lead pay for required checks, because the host holds back the check
/// allowance only from an activation that runs commands itself.
pub fn pays_with_own_shell(policy: &AuthorityGrant, node: &GraphNode) -> bool {
    policy.profiles.iter().any(|profile| {
        profile.definition == node.definition.definition_id.as_str()
            && profile.tools.iter().any(|tool| tool == "bash")
    })
}

pub(super) fn has_shell(policy: &AuthorityGrant) -> bool {
    policy
        .profiles
        .iter()
        .any(|profile| profile.tools.iter().any(|tool| tool == "bash"))
}

/// Whether one host check permission covers this exact claim.
pub(super) fn permission_covers(
    permission: &ConditionPermission,
    record: &ConditionCallRecord,
) -> bool {
    permission.kind
        == (ConditionKind::RepositoryCheck {
            definition: record.definition.clone(),
        })
        && permission.repository == record.repository
        && permission.isolation == record.isolation
        && record
            .run
            .activations
            .iter()
            .all(|activation| permission.nodes.contains(&activation.node_id))
        && record.timeout_ms <= permission.max_timeout_ms
        && record.stdout_bytes <= permission.max_stdout_bytes
        && record.stderr_bytes <= permission.max_stderr_bytes
}

pub(super) fn condition_allowed(
    record: &ConditionCallRecord,
    grant: &GrantRecord,
    policy: &AuthorityGrant,
) -> bool {
    grant.delegated_from.is_none()
        && has_shell(policy)
        && grant
            .host_checks
            .iter()
            .any(|permission| permission_covers(permission, record))
}

/// Follow only the replacement transitions already admitted by this canonical
/// turn. The original permission remains retained for earlier check receipts.
/// The derived permission is committed together with its first actual claim.
pub(super) fn replaced_permission(
    snapshot: &DurableTurnSnapshot,
    record: &ConditionCallRecord,
    permissions: &[ConditionPermission],
    policy: &AuthorityGrant,
) -> Option<ConditionPermission> {
    let contract = snapshot.contract();
    let graph = contract.graph()?;
    let condition = graph
        .conditions
        .iter()
        .find(|condition| condition.condition_id == record.run.condition_id)?;
    let selected = record
        .run
        .activations
        .iter()
        .map(|activation| &activation.node_id)
        .collect::<HashSet<_>>();
    if selected != condition.nodes.iter().collect::<HashSet<_>>() {
        return None;
    }
    for permission in permissions {
        // The same immutable condition had this exact scope in an actual
        // predecessor graph. An unrelated condition or an added Agent is not
        // evidence for extending a check's authorization.
        if !contract.graph_history().iter().any(|revision| {
            revision.previous.conditions.iter().any(|previous| {
                previous.condition_id == record.run.condition_id
                    && previous.kind == permission.kind
                    && previous.nodes == permission.nodes
            })
        }) {
            continue;
        }
        let mut derived = permission.clone();
        for replacement in contract.replaced_nodes() {
            for node in &mut derived.nodes {
                if node == &replacement.previous {
                    *node = replacement.replacement.clone();
                }
            }
        }
        if derived.nodes == permission.nodes || derived.nodes != condition.nodes {
            continue;
        }
        if has_shell(policy) && permission_covers(&derived, record) {
            return Some(derived);
        }
    }
    None
}

pub(super) fn replaced_condition_permission(
    snapshot: &DurableTurnSnapshot,
    record: &ConditionCallRecord,
    grant: &GrantRecord,
) -> Option<ConditionPermission> {
    if grant.delegated_from.is_some() {
        return None;
    }
    replaced_permission(snapshot, record, &grant.host_checks, &grant.policy)
}

/// Stored check permissions are bounded repository checks the original
/// shell-capable approval could run; `why` names the list in the error.
pub(super) fn validate_permissions(
    permissions: &[ConditionPermission],
    original: &AuthorityGrant,
    why: &'static str,
) -> Result<(), AuthorityError> {
    if permissions.len() > MAX_COMPLETION_CONDITIONS {
        return Err(AuthorityError::Capacity);
    }
    for (index, permission) in permissions.iter().enumerate() {
        bounded(&permission.isolation, 128)?;
        let nodes: HashSet<_> = permission.nodes.iter().collect();
        if !matches!(permission.kind, ConditionKind::RepositoryCheck { .. })
            || permission.nodes.is_empty()
            || permission.nodes.len() > MAX_CONTRACT_NODES
            || nodes.len() != permission.nodes.len()
            || permission.max_timeout_ms == 0
            || permission.max_timeout_ms > 180_000
            || permission
                .max_stdout_bytes
                .saturating_add(permission.max_stderr_bytes)
                > 1024 * 1024
            || permissions[..index].contains(permission)
            || !has_shell(original)
        {
            return Err(AuthorityError::Invalid(why));
        }
    }
    Ok(())
}

pub(super) fn validate_host_checks(
    data: &AuthorityData,
    grant: &GrantRecord,
) -> Result<(), AuthorityError> {
    if grant.host_checks.is_empty() {
        return Ok(());
    }
    if grant.delegated_from.is_some() || data.canonical_journal_id.is_none() {
        return Err(AuthorityError::Invalid(
            "required checks are paid by a delegated or uncanonical grant",
        ));
    }
    let original = grant.previous_policies.first().unwrap_or(&grant.policy);
    validate_permissions(
        &grant.host_checks,
        original,
        "invalid required check permission",
    )
}
