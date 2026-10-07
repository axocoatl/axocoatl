//! Keep as PR on the live daemon. Owner: workstream `keep`.
use super::*;
use crate::keep_pr::{KeepPrRequest, KeepPrResponse};

impl AxocoatlDaemon {
    /// `POST /api/sessions/{id}/keep-pr`.
    pub async fn keep_as_pr(
        &self,
        _session_id: &str,
        _request: KeepPrRequest,
    ) -> Result<KeepPrResponse, DaemonError> {
        Err(DaemonError::NotImplemented("keep_as_pr"))
    }
}
