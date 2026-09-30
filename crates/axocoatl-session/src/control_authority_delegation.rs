//! Child budgets are reservations from an already approved supervisor grant.
//! A distinct child grant never clones the supervisor's control capability.
use super::*;
use crate::turn_contract::CommandId;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DelegatedGrantReservation {
    pub parent_grant_id: String,
    pub parent_grant_revision: u64,
    pub parent_activation: ActivationRef,
    pub command_id: CommandId,
    pub template: DefinitionSnapshotRef,
    pub admission_evidence: EvidenceRef,
    pub limits: GrantLimits,
}

impl ControlAuthority {
    /// Privileged native-controller join, after resolving the retained approval,
    /// templates and resource policy. An ordinary serialized policy remains
    /// non-executable through the legacy grant registration path.
    pub fn acknowledge_native_delegation(
        &self,
        snapshot: &DurableTurnSnapshot,
        grant_id: &str,
        expected_revision: u64,
    ) -> Result<(), AuthorityError> {
        let mut state = self.lock()?;
        let index = grant_index(&state.data, grant_id)?;
        let grant = &state.data.grants[index];
        let namespace = self.namespace.as_ref().ok_or(AuthorityError::Denied)?;
        if namespace.identity().journal_id() != snapshot.journal_id()
            || namespace.identity().owner() != snapshot.owner()
            || snapshot.turn_id() != &state.data.turn_id
            || snapshot.contract().state() != Some(LogicalTurnState::Running)
            || grant.policy.delegation.is_none()
            || !snapshot.contract().graph().is_some_and(|graph| {
                graph
                    .nodes
                    .iter()
                    .any(|node| node.node_id == grant.policy.holder)
            })
        {
            return Err(AuthorityError::Denied);
        }
        if grant.native_delegation.as_deref() == Some(snapshot.journal_id()) {
            return Ok(());
        }
        check_revision(&state.data, expected_revision)?;
        let mut next = state.data.clone();
        next.grants[index].native_delegation = Some(snapshot.journal_id().to_owned());
        self.commit(&mut state, next)
    }

    /// Read-only capacity check under the same current parent lease. The actual
    /// reservation repeats all checks and persists before child graph admission.
    pub fn validate_child_grant(
        &self,
        parent: &ActivationLease,
        grant: &AuthorityGrant,
        reservation: &DelegatedGrantReservation,
        now_ms: u64,
    ) -> Result<(), AuthorityError> {
        let state = self.lock()?;
        let next = self.child_grant_candidate(&state.data, parent, grant, reservation, now_ms)?;
        self.prepare_commit(&state.data, next).map(|_| ())
    }
    pub fn reserve_child_grant(
        &self,
        parent: &ActivationLease,
        grant: AuthorityGrant,
        reservation: DelegatedGrantReservation,
        expected_revision: u64,
        now_ms: u64,
    ) -> Result<(), AuthorityError> {
        let mut state = self.lock()?;
        if let Some(existing) = state
            .data
            .grants
            .iter()
            .find(|item| item.policy.id == grant.id)
        {
            return if existing.policy == grant
                && existing.delegated_from.as_ref() == Some(&reservation)
            {
                Ok(())
            } else {
                Err(AuthorityError::Denied)
            };
        }
        check_revision(&state.data, expected_revision)?;
        let next = self.child_grant_candidate(&state.data, parent, &grant, &reservation, now_ms)?;
        self.commit(&mut state, next)
    }
    fn child_grant_candidate(
        &self,
        data: &AuthorityData,
        parent: &ActivationLease,
        grant: &AuthorityGrant,
        reservation: &DelegatedGrantReservation,
        now_ms: u64,
    ) -> Result<AuthorityData, AuthorityError> {
        self.validate_lease(data, parent, now_ms)?;
        validate_grant(grant)?;
        validate_grant_owner(grant, data)?;
        let index = grant_index(data, &parent.grant_id)?;
        let supervisor = &data.grants[index];
        let policy = &supervisor.policy;
        let delegation = policy.delegation.as_ref().ok_or(AuthorityError::Denied)?;
        if supervisor.native_delegation.as_deref() != data.canonical_journal_id.as_deref()
            || supervisor.native_delegation.is_none()
            || parent.activation.node_id != policy.holder
            || reservation.parent_activation != parent.activation
            || reservation.parent_grant_id != parent.grant_id
            || reservation.parent_grant_revision != parent.grant_revision
            || reservation.limits != grant.limits
            || grant.issuer_evidence != reservation.admission_evidence
            || grant.id == policy.id
            || grant.revision != 1
            || grant.delegation.is_some()
            || grant.expires_at_ms > policy.expires_at_ms
            || grant.allow_stop_descendants
            || !grant.descendants.is_empty()
            || !grant.conditions.is_empty()
            || grant.profiles.len() != 1
            || grant.profiles[0].definition != reservation.template.definition_id.as_str()
            || !delegation.templates.contains(&reservation.template)
            || !policy.profiles.contains(&grant.profiles[0])
            || !delegation
                .operations
                .iter()
                .any(|permission| permission.operation == DelegatedOperation::AddAgent)
            || data.grants.iter().any(|item| item.policy.id == grant.id)
            || data.grants.len() >= MAX_GRANTS
        {
            return Err(AuthorityError::Denied);
        }
        let mut next = data.clone();
        let usage = add_reserved_limits(&supervisor.usage, &grant.limits)?;
        if usage.activations > policy.limits.activations
            || usage.invocations > policy.limits.invocations
            || usage.tokens > policy.limits.tokens
            || usage.cost_microunits > policy.limits.cost_microunits
        {
            return Err(AuthorityError::Capacity);
        }
        next.grants[index].usage = usage;
        next.grants.push(GrantRecord {
            policy: grant.clone(),
            previous_policies: vec![],
            expansions: vec![],
            revoked_at_revision: None,
            usage: GrantUsage::default(),
            native_delegation: None,
            delegated_from: Some(reservation.clone()),
            standing: None,
        });
        Ok(next)
    }
    pub fn delegated_parent(
        &self,
        grant_id: &str,
    ) -> Result<Option<DelegatedGrantReservation>, AuthorityError> {
        let state = self.lock()?;
        Ok(state.data.grants[grant_index(&state.data, grant_id)?]
            .delegated_from
            .clone())
    }
}

pub(super) fn add_reserved_limits(
    usage: &GrantUsage,
    limits: &GrantLimits,
) -> Result<GrantUsage, AuthorityError> {
    Ok(GrantUsage {
        activations: usage
            .activations
            .checked_add(limits.activations)
            .ok_or(AuthorityError::Capacity)?,
        invocations: usage
            .invocations
            .checked_add(limits.invocations)
            .ok_or(AuthorityError::Capacity)?,
        tokens: usage
            .tokens
            .checked_add(limits.tokens)
            .ok_or(AuthorityError::Capacity)?,
        cost_microunits: usage
            .cost_microunits
            .checked_add(limits.cost_microunits)
            .ok_or(AuthorityError::Capacity)?,
    })
}
pub(super) fn delegated_ancestors_live(
    data: &AuthorityData,
    grant: &GrantRecord,
    now_ms: u64,
) -> bool {
    let mut current = grant;
    for _ in 0..MAX_GRANTS {
        let Some(reservation) = &current.delegated_from else {
            return true;
        };
        let Ok(index) = grant_index(data, &reservation.parent_grant_id) else {
            return false;
        };
        current = &data.grants[index];
        if current.revoked_at_revision.is_some()
            || !super::expansion::revision_path(
                current,
                reservation.parent_grant_revision,
                current.policy.revision,
            )
            || now_ms >= current.policy.expires_at_ms
        {
            return false;
        }
    }
    false
}
pub(super) fn validate_delegated_records(data: &AuthorityData) -> Result<(), AuthorityError> {
    for grant in &data.grants {
        if grant.native_delegation.is_some()
            && (grant.policy.delegation.is_none()
                || grant.native_delegation != data.canonical_journal_id)
        {
            return Err(AuthorityError::Invalid(
                "foreign native delegation admission",
            ));
        }
        let Some(reservation) = &grant.delegated_from else {
            continue;
        };
        if grant.policy.id == reservation.parent_grant_id
            || reservation.parent_activation.session_id != data.session_id
            || reservation.parent_activation.turn_id != data.turn_id
        {
            return Err(AuthorityError::Invalid("foreign delegated grant owner"));
        }
        let parent = &data.grants[grant_index(data, &reservation.parent_grant_id)?];
        let policy = parent
            .previous_policies
            .iter()
            .chain(std::iter::once(&parent.policy))
            .find(|policy| policy.revision == reservation.parent_grant_revision)
            .ok_or(AuthorityError::Invalid("missing delegated parent revision"))?;
        let original = grant.previous_policies.first().unwrap_or(&grant.policy);
        if parent.native_delegation.is_none()
            || parent.native_delegation != data.canonical_journal_id
            || policy.holder != reservation.parent_activation.node_id
            || !data.activations.iter().any(|activation| {
                activation.activation == reservation.parent_activation
                    && activation.grant_id == reservation.parent_grant_id
                    && expansion::recorded_revision(
                        data,
                        &reservation.parent_grant_id,
                        activation.grant_revision,
                        reservation.parent_grant_revision,
                    )
            })
            || original.limits != reservation.limits
            || original.issuer_evidence != reservation.admission_evidence
            || original.expires_at_ms > policy.expires_at_ms
            || original.profiles.len() != 1
            || !policy.profiles.contains(&original.profiles[0])
            || original.profiles[0].definition != reservation.template.definition_id.as_str()
            || !policy
                .delegation
                .as_ref()
                .is_some_and(|delegation| delegation.templates.contains(&reservation.template))
        {
            return Err(AuthorityError::Invalid(
                "invalid delegated grant reservation",
            ));
        }
        let mut seen = HashSet::new();
        let mut cursor = grant;
        while let Some(parent) = &cursor.delegated_from {
            if !seen.insert(&cursor.policy.id) {
                return Err(AuthorityError::Invalid("cyclic delegated grants"));
            }
            cursor = &data.grants[grant_index(data, &parent.parent_grant_id)?];
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::turn_contract::{ActivationId, AgentDefinitionId, ExecutionEpochId};

    #[test]
    fn delegated_child_accepts_only_recorded_parent_expansion_lineage() {
        let activation = ActivationRef {
            session_id: SessionId::new("session").unwrap(),
            turn_id: LogicalTurnId::new("turn").unwrap(),
            execution_epoch_id: ExecutionEpochId::new("epoch").unwrap(),
            node_id: TurnNodeId::new("parent").unwrap(),
            generation: 1,
            activation_id: ActivationId::new("parent-activation").unwrap(),
        };
        let template = DefinitionSnapshotRef {
            definition_id: AgentDefinitionId::new("worker").unwrap(),
            snapshot: EvidenceRef::new("template-evidence").unwrap(),
        };
        let original: AuthorityGrant = serde_json::from_value(serde_json::json!({
            "id":"parent-grant", "revision":1,"issuer_evidence":"human-approval",
            "holder":"parent","descendants":[],"allow_stop_descendants":false,
            "profiles":[{"definition":"worker","provider":"local","model":"finite","isolation":"in-process","tools":[]}],
            "conditions":[],"limits":{"activations":8,"invocations":16,"tokens":1000,"cost_microunits":0},"expires_at_ms":1000,
            "delegation":{
                "schema_version":1,"scope":{"session_id":"session","turn_id":"turn","task":"task-evidence","approved_graph":"graph-evidence"},
                "operations":[],"templates":[template],"resource_policy":"resource-evidence",
                "graph_limits":{"max_nodes":8,"max_edges":8},"required_conditions":[],"completion_criteria":[],"machine_blockers":[],
                "replay_policy":"require_proved_effect_safety"
            }
        })).unwrap();
        let mut expanded = original.clone();
        expanded.revision = 2;
        expanded.limits.tokens = 2000;
        let limits = GrantLimits {
            activations: 1,
            invocations: 2,
            tokens: 200,
            cost_microunits: 0,
        };
        let reservation = DelegatedGrantReservation {
            parent_grant_id: original.id.clone(),
            parent_grant_revision: 2,
            parent_activation: activation.clone(),
            command_id: CommandId::new("add-child").unwrap(),
            template,
            admission_evidence: EvidenceRef::new("child-admission").unwrap(),
            limits: limits.clone(),
        };
        let child = AuthorityGrant {
            id: "child-grant".into(),
            revision: 1,
            issuer_evidence: reservation.admission_evidence.clone(),
            holder: TurnNodeId::new("child").unwrap(),
            descendants: vec![],
            allow_stop_descendants: false,
            delegation: None,
            profiles: original.profiles.clone(),
            conditions: vec![],
            limits,
            expires_at_ms: 1000,
        };
        let profile = original.profiles[0].clone();
        let record = |policy, native_delegation, delegated_from| GrantRecord {
            policy,
            previous_policies: vec![],
            expansions: vec![],
            revoked_at_revision: None,
            usage: GrantUsage::default(),
            native_delegation,
            delegated_from,
            standing: None,
        };
        let mut parent = record(expanded, Some("journal".into()), None);
        parent.previous_policies.push(original.clone());
        parent.expansions.push(expansion::ApprovedExpansion {
            revision: 2,
            evidence: EvidenceRef::new("exact-human-expansion").unwrap(),
        });
        let mut data = AuthorityData {
            schema_version: 1,
            session_id: activation.session_id.clone(),
            turn_id: activation.turn_id.clone(),
            revision: 5,
            closed: true,
            lifecycle_suspension: None,
            grants: vec![parent, record(child, None, Some(reservation))],
            activations: vec![ActivationRecord {
                activation,
                grant_id: original.id.clone(),
                grant_revision: 1,
                profile,
                stopped: true,
                provider_gated: true,
                never_dispatched: false,
            }],
            claims: vec![],
            provider_calls: vec![],
            condition_calls: vec![],
            canonical_journal_id: Some("journal".into()),
        };
        // Closing does not rewrite the original activation registration. A child
        // admitted after approved expansion must still validate on durable read.
        validate_delegated_records(&data).unwrap();
        assert_eq!(data.activations[0].grant_revision, 1);
        let saved = data.grants[0].expansions.clone();
        data.grants[0].expansions.clear();
        assert!(
            validate_delegated_records(&data).is_err(),
            "policy revision alone is not an authenticated expansion"
        );
        data.grants[0].expansions = saved;
        data.activations[0].activation.activation_id =
            ActivationId::new("foreign-activation").unwrap();
        assert!(
            validate_delegated_records(&data).is_err(),
            "another activation cannot supply expansion lineage"
        );
    }
}
