//! External agent activations on the live daemon: run the program in the
//! Session container as the writer user under `--harden`, with the run's
//! routes, and record its parsed output as the activation's evidence.
//! Owner: workstream `agents`.
use super::*;
use crate::external_agent::{ExternalActivationRequest, ExternalActivationResult};

impl AxocoatlDaemon {
    pub async fn run_external_activation(
        &self,
        _request: ExternalActivationRequest,
    ) -> Result<ExternalActivationResult, DaemonError> {
        Err(DaemonError::NotImplemented("run_external_activation"))
    }
}
