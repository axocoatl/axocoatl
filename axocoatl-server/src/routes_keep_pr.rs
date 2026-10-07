//! `POST /api/sessions/{id}/keep-pr`. Owner: workstream `keep`.
//!
//! Statuses: 200 with `KeepPrResponse`; 422 for a malformed request (unknown
//! field, bad run id, branch, remote or title); 409 when the run or the
//! repository refuses (not this Session's run, not finished, not passed, a
//! run path dirty before the run, the branch exists locally or on the remote,
//! the branch is the remote's default branch, HEAD moved); 501 while a hook
//! it needs is not implemented; 400 for anything else (Session not found,
//! host git or gh failed).
use super::*;
use axocoatl_daemon::keep_pr::{KeepPrRequest, KeepPrResponse};

pub async fn keep_pr(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(request): Json<KeepPrRequest>,
) -> Result<Json<KeepPrResponse>, (StatusCode, Json<ErrorResponse>)> {
    state
        .read()
        .await
        .keep_as_pr(&id, request)
        .await
        .map(Json)
        .map_err(attempt_err)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axocoatl_daemon::keep_pr::KeepPrError;
    use axocoatl_daemon::DaemonError;

    #[test]
    fn keep_errors_have_their_statuses() {
        for (error, status, words) in [
            (
                KeepPrError::Refused("run did not pass".into()),
                StatusCode::CONFLICT,
                "run did not pass",
            ),
            (
                KeepPrError::Invalid("bad branch".into()),
                StatusCode::UNPROCESSABLE_ENTITY,
                "bad branch",
            ),
            (
                KeepPrError::NotImplemented("RunRecordStore::open"),
                StatusCode::NOT_IMPLEMENTED,
                "RunRecordStore::open",
            ),
            (
                KeepPrError::Git("push failed".into()),
                StatusCode::BAD_REQUEST,
                "push failed",
            ),
            (
                KeepPrError::AfterBranch {
                    branch: "axocoatl/fix-1".into(),
                    commit: "c".repeat(40),
                    message: "gh pr create failed".into(),
                },
                StatusCode::BAD_REQUEST,
                "created branch axocoatl/fix-1",
            ),
        ] {
            let (got, Json(body)) = attempt_err(DaemonError::from(error));
            assert_eq!(got, status, "{words}");
            assert!(body.error.contains(words), "{} / {words}", body.error);
        }
    }

    async fn post(app: &axum::Router, path: &str, body: &str) -> (StatusCode, String) {
        use tower::ServiceExt;
        let response = app
            .clone()
            .oneshot(
                axum::http::Request::post(path)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(axum::body::Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    /// The registered handler on a live daemon: the body is checked before
    /// anything is read, and an unknown Session is refused.
    #[cfg(unix)]
    #[tokio::test]
    async fn keep_pr_route_validates_the_request_on_a_live_daemon() {
        use std::os::unix::fs::PermissionsExt;
        const CHILD: &str = "AXOCOATL_TEST_KEEP_PR_ROUTE_CHILD";
        const RUN: &str = "run-0f8c1a2b-3c4d-4e5f-8a9b-0c1d2e3f4a5b";
        if std::env::var_os(CHILD).is_some() {
            let mut config = axocoatl_config::AxocoatlConfig::default();
            config.agents.clear();
            config.consolidation.enabled = false;
            let daemon = axocoatl_daemon::AxocoatlDaemon::bootstrap_headless(config)
                .await
                .unwrap();
            let state: AppState = Arc::new(tokio::sync::RwLock::new(daemon));
            let app = axum::Router::new()
                .route("/api/sessions/{id}/keep-pr", axum::routing::post(keep_pr))
                .with_state(state.clone());
            let path = "/api/sessions/ses-00000000-0000-4000-8000-000000000000/keep-pr";

            let (status, _) =
                post(&app, path, &format!(r#"{{"run_id":"{RUN}","force":true}}"#)).await;
            assert_eq!(
                status,
                StatusCode::UNPROCESSABLE_ENTITY,
                "unknown fields are refused"
            );
            for body in [
                r#"{"run_id":"run-1"}"#.to_string(),
                format!(r#"{{"run_id":"{RUN}","branch":"-x"}}"#),
                format!(r#"{{"run_id":"{RUN}","remote":"a b","open_pr":true}}"#),
                format!(r#"{{"run_id":"{RUN}","title":"two\nlines"}}"#),
            ] {
                let (status, text) = post(&app, path, &body).await;
                assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}: {text}");
                let error: serde_json::Value = serde_json::from_str(&text).unwrap();
                assert!(
                    error["error"].as_str().unwrap().contains("keep as PR"),
                    "{text}"
                );
            }
            let (status, text) = post(&app, path, &format!(r#"{{"run_id":"{RUN}"}}"#)).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{text}");
            assert!(text.contains("not found"), "{text}");
            state.read().await.shutdown().await.unwrap();
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let bin = root.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let podman = bin.join("podman");
        std::fs::write(
            &podman,
            r#"#!/bin/sh
case "$*" in
  --version) printf 'podman version 5.0.0\n' ;;
  'machine list --format json') printf '[{"Running":true}]\n' ;;
  'info --format json') printf '{}\n' ;;
  'ps '*) ;;
  *) printf 'unexpected Podman command: %s\n' "$*" >&2; exit 1 ;;
esac
"#,
        )
        .unwrap();
        std::fs::set_permissions(&podman, std::fs::Permissions::from_mode(0o700)).unwrap();
        let output = tokio::time::timeout(
            std::time::Duration::from_secs(120),
            tokio::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "routes::keep_pr_routes::tests::keep_pr_route_validates_the_request_on_a_live_daemon",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .env("AXOCOATL_DATA_DIR", root.path().join("data"))
                .env("AXOCOATL_SOCKET_PATH", root.path().join("ipc/daemon.sock"))
                .env("PATH", &bin)
                .current_dir(root.path())
                .kill_on_drop(true)
                .output(),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
