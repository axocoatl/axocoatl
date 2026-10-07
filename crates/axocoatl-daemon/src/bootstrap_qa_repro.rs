//! Host-run reproductions for the qa loadout: one `browser_check` of one
//! repository test file against one base URL, in the browser container,
//! outside any Agent's tool loop, recorded in the Session's network record
//! like any browser call. Owner: workstream `review-qa`.
use super::*;
use crate::loadout::host::ReproRequest;
use axocoatl_session::run_outcome::ReproRun;

impl AxocoatlDaemon {
    /// Run `request`'s reproduction once against its base URL in the
    /// Session's browser container (see
    /// [`crate::session_dispatch_browser::BrowserService::run_repro`]). The
    /// test file is read from the Session's Workspace, the checkout the
    /// explorer wrote it to. A reproduction that cannot run is an `error` run
    /// with its reason; an unknown Session, a Workspace that is not the
    /// Session's, or a daemon without the browser tools is an error.
    pub async fn run_repro_check(
        &self,
        session_id: &str,
        request: &ReproRequest,
    ) -> Result<ReproRun, DaemonError> {
        let browser = self.browser_service.clone().ok_or_else(|| {
            DaemonError::Session(
                "the browser tools are not configured: add a browser: block to the \
                 configuration, so qa reproductions can run"
                    .to_string(),
            )
        })?;
        let session = self
            .get_session(session_id)
            .await
            .ok_or_else(|| DaemonError::Session(format!("session '{session_id}' not found")))?;
        let workspace = self
            .workspace_store
            .lock()
            .await
            .get(&session.workspace_id)
            .ok_or_else(|| {
                DaemonError::Session(format!(
                    "the Workspace of session '{session_id}' is missing"
                ))
            })?;
        if workspace.canonical_path != session.working_dir {
            return Err(DaemonError::Session(
                "the Session's path differs from its Workspace".to_string(),
            ));
        }
        match axocoatl_tools::browser_tool::normalize_repo_path(&request.path) {
            Ok(path) if path == request.path => {}
            Ok(_) | Err(_) => {
                return Ok(ReproRun {
                    base_url: request.base_url.clone(),
                    status: "error".into(),
                    first_error: Some(format!(
                        "{:?} is not a repository path browser_check can read",
                        request.path
                    )),
                })
            }
        }
        let checkout = SecureDir::open(&workspace.canonical_path)
            .map_err(|error| DaemonError::Session(error.to_string()))?;
        Ok(browser
            .run_repro(
                session_id,
                checkout,
                &request.path,
                &request.base_url,
                request.timeout_ms,
            )
            .await)
    }
}
