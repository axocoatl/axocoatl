//! Host-run reproductions for the qa loadout: one `browser_check` of one
//! repository test file against one base URL, in the browser container,
//! outside any Agent's tool loop, recorded in the Session's network record
//! like any browser call. Owner: workstream `review-qa`.
use super::*;
use crate::loadout::host::ReproRequest;
use axocoatl_session::run_outcome::ReproRun;

impl AxocoatlDaemon {
    pub async fn run_repro_check(
        &self,
        _session_id: &str,
        _request: &ReproRequest,
    ) -> Result<ReproRun, DaemonError> {
        Err(DaemonError::NotImplemented("run_repro_check"))
    }
}
