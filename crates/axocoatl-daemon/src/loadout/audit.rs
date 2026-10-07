//! The audit loadout: a plan turn (one planner, AREAS JSON), then one turn
//! of one fresh read-only worker per area in parallel and an integrator
//! that depends on all of them. Owner: audit.

use async_trait::async_trait;

use super::{KindDriver, KindReport, RunContext, RunError, RunHost};

pub struct AuditDriver;

#[async_trait]
impl KindDriver for AuditDriver {
    async fn drive(&self, _host: &dyn RunHost, _run: &RunContext) -> Result<KindReport, RunError> {
        Err(RunError::NotImplemented("loadout::audit::AuditDriver"))
    }
}
