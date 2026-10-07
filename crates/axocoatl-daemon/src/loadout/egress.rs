//! A loadout Session's egress policy: the loadout's `egress.allow` and
//! `routes` (and the routes its external Agents and e2e checks need) added
//! to the daemon's own lists for that Session only. Owner: core.

use axocoatl_config::loadout::ResolvedLoadout;

use super::RunError;
use crate::session_egress::EgressPolicyConfig;

/// The policy of the run's Session: `base` plus the loadout's entries.
/// Credentials named by routes resolve from `credentials` first, then from
/// the secret store (`crate::secret_store`).
pub fn loadout_policy(
    _base: &EgressPolicyConfig,
    _resolved: &ResolvedLoadout,
    _secrets_dir: &std::path::Path,
) -> Result<EgressPolicyConfig, RunError> {
    Err(RunError::NotImplemented("loadout::egress::loadout_policy"))
}
