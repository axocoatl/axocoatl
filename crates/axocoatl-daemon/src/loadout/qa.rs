//! The qa loadout: one browser explorer; each finding's reproduction re-run
//! against the build under test and the reference; coverage as not covered.
//! Owner: review-qa.

use async_trait::async_trait;

use super::{KindDriver, KindReport, RunContext, RunError, RunHost};

pub struct QaDriver;

#[async_trait]
impl KindDriver for QaDriver {
    async fn drive(&self, _host: &dyn RunHost, _run: &RunContext) -> Result<KindReport, RunError> {
        Err(RunError::NotImplemented("loadout::qa::QaDriver"))
    }
}
