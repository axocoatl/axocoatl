//! One standing grant allowance across its linked native turns.
use super::*;
use crate::control_authority::{AuthorityGrant, GrantUsage};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TeamWorkGrantAllocation {
    pub grant: TeamWorkGrantReference,
    pub consumed_before: GrantUsage,
    pub(crate) settlement: Option<Settlement>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Settlement {
    pub(crate) total_consumed: GrantUsage,
    pub(crate) revoked: bool,
    #[serde(default, skip_serializing_if = "SettlementBasis::is_measured")]
    pub(crate) basis: SettlementBasis,
}

/// How a settlement established what the work consumed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SettlementBasis {
    /// Every provider call reported complete usage.
    #[default]
    Measured,
    /// A person chose to charge each call whose usage stayed unknown at its
    /// full admitted reservation. This can only over-count; the calls' own
    /// recorded usage remains unknown.
    ReservedCeiling { unknown_calls: u32 },
}

impl SettlementBasis {
    pub fn is_measured(&self) -> bool {
        matches!(self, SettlementBasis::Measured)
    }
}

/// A person's decision to settle work whose usage is unknown at its ceiling.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CeilingDecision {
    pub decided_at_ms: u64,
}

/// Minted from an acknowledged inbox reservation, never deserialized input.
#[derive(Clone)]
pub struct DurableTeamWorkAllocation {
    pub(crate) receipt_id: String,
    pub(crate) session_id: String,
    pub(crate) turn_id: String,
    pub(crate) allocation: TeamWorkGrantAllocation,
    pub(crate) required_checks: Vec<Vec<String>>,
}

/// Minted only from the actual closed authority journal and canonical turn.
pub struct TeamWorkGrantSettlement {
    pub(crate) receipt_id: String,
    pub(crate) session_id: String,
    pub(crate) turn_id: String,
    pub(crate) grant: TeamWorkGrantReference,
    pub(crate) consumed_before: GrantUsage,
    pub(crate) settled: Settlement,
}

impl TeamWorkGrantSettlement {
    pub fn basis(&self) -> SettlementBasis {
        self.settled.basis
    }
    pub fn consumed_before(&self) -> &GrantUsage {
        &self.consumed_before
    }
    pub fn total_consumed(&self) -> &GrantUsage {
        &self.settled.total_consumed
    }
}

impl TeamWorkGrantAllocation {
    /// How this allocation's settlement was established, once settled.
    pub fn settlement_basis(&self) -> Option<SettlementBasis> {
        self.settlement.as_ref().map(|settlement| settlement.basis)
    }

    pub fn is_settled(&self) -> bool {
        self.settlement.is_some()
    }
}

impl DurableTeamWorkAllocation {
    pub fn grant_id(&self) -> &str {
        &self.allocation.grant.id
    }
    pub(crate) fn verify(&self, session: &str, turn: &str, policy: &AuthorityGrant) -> bool {
        self.session_id == session
            && self.turn_id == turn
            && self.allocation.grant.id == policy.id
            && self.allocation.grant.revision == policy.revision
            && self.allocation.grant.limits == policy.limits
            && self.allocation.grant.expires_at_ms == policy.expires_at_ms
    }
}

fn within(used: &GrantUsage, limit: &crate::control_authority::GrantLimits) -> bool {
    used.activations <= limit.activations
        && used.invocations <= limit.invocations
        && used.tokens <= limit.tokens
        && used.cost_microunits <= limit.cost_microunits
}
fn monotone(old: &GrantUsage, new: &GrantUsage) -> bool {
    old.activations <= new.activations
        && old.invocations <= new.invocations
        && old.tokens <= new.tokens
        && old.cost_microunits <= new.cost_microunits
}

impl TeamWorkInbox {
    /// Allocate the binding's grants, or the named subset a targeted turn
    /// actually installs. Lineage stays per grant across every receipt.
    pub(super) fn allocate_native_budget(
        &self,
        index: usize,
    ) -> Result<Vec<TeamWorkGrantAllocation>, TeamWorkError> {
        let receipt = &self.data.receipts[index];
        if !receipt.native_capacity_reserved {
            return Err(TeamWorkError::DispositionConflict);
        }
        let binding = self
            .data
            .bindings
            .iter()
            .find(|binding| binding.binding == receipt.request.binding)
            .ok_or_else(|| TeamWorkError::Invalid("native work lost its binding budget".into()))?;
        let mut allocated = Vec::new();
        for grant in &binding.grants {
            let mut consumed = GrantUsage::default();
            for (prior_index, earlier) in self.data.receipts.iter().enumerate() {
                if prior_index == index {
                    continue;
                }
                if earlier.request.binding.session_id != receipt.request.binding.session_id {
                    continue;
                }
                for allocation in earlier
                    .allocations
                    .iter()
                    .filter(|allocation| allocation.grant.id == grant.id)
                {
                    if prior_index > index {
                        return Err(TeamWorkError::Invalid(
                            "standing grant allocation is out of receipt order".into(),
                        ));
                    }
                    if allocation.grant != *grant {
                        return Err(TeamWorkError::EventConflict);
                    }
                    let Some(settled) = &allocation.settlement else {
                        return Err(TeamWorkError::Invalid(
                            "standing grant has an unresolved reserved turn".into(),
                        ));
                    };
                    if settled.revoked {
                        return Err(TeamWorkError::Invalid("standing grant was revoked".into()));
                    }
                    consumed = settled.total_consumed.clone();
                }
            }
            if !within(&consumed, &grant.limits)
                || consumed.activations >= grant.limits.activations
                || consumed.invocations >= grant.limits.invocations
                || consumed.tokens >= grant.limits.tokens
            {
                return Err(TeamWorkError::Invalid(
                    "standing grant budget is exhausted".into(),
                ));
            }
            allocated.push(TeamWorkGrantAllocation {
                grant: grant.clone(),
                consumed_before: consumed,
                settlement: None,
            });
        }
        Ok(allocated)
    }

    pub fn native_allocations(
        &self,
        receipt_id: &str,
    ) -> Result<Vec<DurableTeamWorkAllocation>, TeamWorkError> {
        self.ensure_usable()?;
        let receipt = &self.data.receipts[self.index(receipt_id)?];
        if receipt.disposition != TeamWorkDisposition::Reserved
            || receipt.execution_source.is_none()
            || receipt.allocations.is_empty()
        {
            return Err(TeamWorkError::DispositionConflict);
        }
        let binding = self
            .data
            .bindings
            .iter()
            .find(|binding| binding.binding == receipt.request.binding)
            .ok_or_else(|| TeamWorkError::Invalid("standing binding is missing".into()))?;
        Ok(receipt
            .allocations
            .iter()
            .map(|allocation| DurableTeamWorkAllocation {
                receipt_id: receipt.receipt_id.clone(),
                session_id: receipt.request.binding.session_id.clone(),
                turn_id: receipt.turn_id.clone(),
                allocation: allocation.clone(),
                required_checks: binding.required_checks.clone(),
            })
            .collect())
    }

    /// Record that a person accepts charging this work's unknown provider
    /// usage at its reserved ceiling. Settlement itself still comes from the
    /// closed authority journal; repeating the decision changes nothing.
    pub fn record_ceiling_decision(
        &mut self,
        receipt_id: &str,
        decided_at_ms: u64,
    ) -> Result<TeamWorkReceipt, TeamWorkError> {
        self.ensure_usable()?;
        let index = self.index(receipt_id)?;
        let receipt = &self.data.receipts[index];
        if receipt.ceiling_decision.is_some() {
            return Ok(receipt.clone());
        }
        if receipt.disposition != TeamWorkDisposition::Reserved
            || receipt.allocations.is_empty()
            || receipt
                .allocations
                .iter()
                .all(TeamWorkGrantAllocation::is_settled)
        {
            return Err(TeamWorkError::DispositionConflict);
        }
        let mut next = self.data.clone();
        next.receipts[index].ceiling_decision = Some(CeilingDecision { decided_at_ms });
        let result = next.receipts[index].clone();
        self.commit(next)?;
        Ok(result)
    }

    pub fn settle_native_budget(
        &mut self,
        receipt_id: &str,
        settlements: &[TeamWorkGrantSettlement],
    ) -> Result<TeamWorkReceipt, TeamWorkError> {
        let (next, index) = self.staged_settlement(receipt_id, settlements)?;
        let result = next.receipts[index].clone();
        self.commit(next)?;
        Ok(result)
    }

    /// Whether these settlements would be accepted, without recording them. A
    /// settlement past the grant's reviewed limits (possible at the ceiling
    /// after a grant was expanded) is refused rather than persisted.
    pub fn check_native_settlement(
        &self,
        receipt_id: &str,
        settlements: &[TeamWorkGrantSettlement],
    ) -> Result<(), TeamWorkError> {
        self.staged_settlement(receipt_id, settlements).map(|_| ())
    }

    fn staged_settlement(
        &self,
        receipt_id: &str,
        settlements: &[TeamWorkGrantSettlement],
    ) -> Result<(InboxData, usize), TeamWorkError> {
        self.ensure_usable()?;
        let index = self.index(receipt_id)?;
        let receipt = &self.data.receipts[index];
        if settlements.len() != receipt.allocations.len() || settlements.is_empty() {
            return Err(TeamWorkError::DispositionConflict);
        }
        let mut next = self.data.clone();
        for (allocation, settlement) in next.receipts[index].allocations.iter_mut().zip(settlements)
        {
            if settlement.receipt_id != receipt.receipt_id
                || settlement.session_id != receipt.request.binding.session_id
                || settlement.turn_id != receipt.turn_id
                || settlement.grant != allocation.grant
                || settlement.consumed_before != allocation.consumed_before
                || allocation
                    .settlement
                    .as_ref()
                    .is_some_and(|old| old != &settlement.settled)
            {
                return Err(TeamWorkError::EventConflict);
            }
            allocation.settlement = Some(settlement.settled.clone());
        }
        validate_allocations(&next)?;
        Ok((next, index))
    }
}

pub(super) fn validate_allocations(data: &InboxData) -> Result<(), TeamWorkError> {
    let mut prior = std::collections::HashMap::<
        (&str, &str),
        (&TeamWorkGrantReference, Option<&Settlement>),
    >::new();
    for receipt in &data.receipts {
        if receipt.allocations.len() > crate::turn_contract::MAX_CONTRACT_NODES {
            return Err(TeamWorkError::Capacity);
        }
        if receipt.allocations.is_empty() {
            continue;
        }
        if (receipt.disposition != TeamWorkDisposition::Reserved && receipt.never_begun.is_none())
            || !receipt.native_capacity_reserved
            || receipt.execution_source.is_none()
        {
            return Err(TeamWorkError::DispositionConflict);
        }
        let binding = data
            .bindings
            .iter()
            .find(|binding| binding.binding == receipt.request.binding)
            .ok_or_else(|| TeamWorkError::Invalid("budget allocation lost its binding".into()))?;
        if receipt.allocations.len() > binding.grants.len() {
            return Err(TeamWorkError::DispositionConflict);
        }
        // Whole-team work allocates every grant; targeted work an ordered subset.
        let mut cursor = 0;
        for allocation in &receipt.allocations {
            let offset = binding.grants[cursor..]
                .iter()
                .position(|grant| grant.id == allocation.grant.id)
                .ok_or(TeamWorkError::DispositionConflict)?;
            let grant = &binding.grants[cursor + offset];
            cursor += offset + 1;
            let key = (
                receipt.request.binding.session_id.as_str(),
                grant.id.as_str(),
            );
            let before = match prior.get(&key) {
                None => GrantUsage::default(),
                Some((old, Some(previous))) if *old == grant && !previous.revoked => {
                    previous.total_consumed.clone()
                }
                _ => {
                    return Err(TeamWorkError::Invalid(
                        "overlapping or revoked standing grant allocation".into(),
                    ))
                }
            };
            if allocation.grant != *grant
                || allocation.consumed_before != before
                || !within(&before, &grant.limits)
            {
                return Err(TeamWorkError::Invalid(
                    "standing budget lineage differs".into(),
                ));
            }
            if allocation.settlement.as_ref().is_some_and(|settled| {
                !monotone(&before, &settled.total_consumed)
                    || !within(&settled.total_consumed, &grant.limits)
            }) {
                return Err(TeamWorkError::Invalid(
                    "standing budget settlement differs".into(),
                ));
            }
            prior.insert(key, (grant, allocation.settlement.as_ref()));
        }
    }
    Ok(())
}
