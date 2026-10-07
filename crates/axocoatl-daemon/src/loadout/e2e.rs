//! The tester-army/e2e required check: expansion of a loadout `e2e` check
//! into an exact argv and options, and collection of its report after the
//! turn. Owner: e2e.

use axocoatl_config::loadout::{E2eCheck, ResolvedLoadout};
use axocoatl_session::check_options::RequiredCheckOptions;
use axocoatl_session::run_outcome::CheckResult;

use super::{RunContext, RunError, RunHost};

/// The npm package and version the check runs.
pub const E2E_PACKAGE: &str = "e2e@0.18.0";
/// Environment the check always sets.
pub const E2E_FORCED_ENV: [(&str, &str); 1] = [("E2E_TELEMETRY_DISABLED", "1")];

/// The argv and options of one `e2e` check.
pub fn expand_e2e_check(
    _name: &str,
    _check: &E2eCheck,
    _timeout_secs: u64,
    _resolved: &ResolvedLoadout,
) -> Result<(Vec<String>, RequiredCheckOptions), RunError> {
    Err(RunError::NotImplemented("loadout::e2e::expand_e2e_check"))
}

/// Read each check's bound report from the Session container and attach it
/// to its result.
pub async fn collect_reports(
    _host: &dyn RunHost,
    _run: &RunContext,
    _checks: &mut [CheckResult],
) -> Result<(), RunError> {
    Err(RunError::NotImplemented("loadout::e2e::collect_reports"))
}
