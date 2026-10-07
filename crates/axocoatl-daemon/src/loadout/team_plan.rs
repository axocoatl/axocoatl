//! Resolved loadout → `SessionTeamEdit` (slots, dependencies, required
//! checks with options, required review, grants). Owner: core.

use axocoatl_config::loadout::{LoadoutAgent, ModelSpec, ResolvedLoadout};

use super::RunError;
use crate::SessionTeamEdit;

/// One slot to create: a loadout Agent, possibly instantiated per area.
#[derive(Debug, Clone, PartialEq)]
pub struct SlotPlan {
    /// Team slot id, such as `writer` or `worker-auth`.
    pub slot_id: String,
    pub agent: LoadoutAgent,
    pub model: ModelSpec,
    /// Replaces the Agent's instructions (audit area workers).
    pub instructions: Option<String>,
    /// Slot ids this slot depends on.
    pub depends_on: Vec<String>,
    pub required: bool,
}

/// The slots of the loadout's own Agents, in declaration order.
pub fn default_slots(_resolved: &ResolvedLoadout) -> Result<Vec<SlotPlan>, RunError> {
    Err(RunError::NotImplemented(
        "loadout::team_plan::default_slots",
    ))
}

/// The edit that applies `slots` with the loadout's checks, review and
/// budgets. `with_checks_and_review` is false for an audit's plan turn.
pub fn team_edit(
    _resolved: &ResolvedLoadout,
    _slots: &[SlotPlan],
    _with_checks_and_review: bool,
    _expected_configuration_revision: u64,
) -> Result<SessionTeamEdit, RunError> {
    Err(RunError::NotImplemented("loadout::team_plan::team_edit"))
}
