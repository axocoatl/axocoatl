//! The generic run driver: prepare, drive the kind, build the Outcome.
//! Owner: core.

use async_trait::async_trait;
use axocoatl_session::run_outcome::{RunOutcome, TurnObservation};

use super::{KindDriver, KindReport, RunContext, RunError, RunHost};

/// One turn of the resolved team; the driver of `custom` loadouts and the
/// building block of `fix` and `qa`.
pub struct SingleTurnDriver;

#[async_trait]
impl KindDriver for SingleTurnDriver {
    async fn drive(&self, host: &dyn RunHost, run: &RunContext) -> Result<KindReport, RunError> {
        let turn = run_single_turn(host, run).await?;
        Ok(KindReport {
            turns: vec![turn],
            ..KindReport::default()
        })
    }
}

/// Apply the loadout's team, send its prompt, and wait for the turn.
pub async fn run_single_turn(
    _host: &dyn RunHost,
    _run: &RunContext,
) -> Result<TurnObservation, RunError> {
    Err(RunError::NotImplemented("loadout::driver::run_single_turn"))
}

/// Run `run` to its Outcome: drive its kind, fold the report and observed
/// turns into a [`RunOutcome`], decide the verdict, and record it.
pub async fn run_to_outcome(
    _host: &dyn RunHost,
    _run: &RunContext,
) -> Result<RunOutcome, RunError> {
    Err(RunError::NotImplemented("loadout::driver::run_to_outcome"))
}
