//! The fix loadout: one turn of one writer with required checks and a
//! required review by a different model; adjudications from the writer's
//! answers. Owner: review-qa.

use async_trait::async_trait;

use super::{KindDriver, KindReport, RunContext, RunError, RunHost};

pub struct FixDriver;

#[async_trait]
impl KindDriver for FixDriver {
    async fn drive(&self, _host: &dyn RunHost, _run: &RunContext) -> Result<KindReport, RunError> {
        Err(RunError::NotImplemented("loadout::fix::FixDriver"))
    }
}
