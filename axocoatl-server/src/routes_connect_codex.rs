//! `POST /api/connect/codex/refresh`: refresh the stored ChatGPT sign-in
//! now, for `axocoatl connect codex --refresh`. Owner: workstream `agents`.
//!
//! Only an authenticated caller on this computer gets it: the route is
//! refused (`403`) when the daemon accepts API requests without
//! authentication (`allow_unauthenticated`) and for a peer that is not on a
//! loopback address, besides the API authentication every route has. The
//! body is a [`RefreshReport`] (its `outcome` says what happened) and never
//! holds a token. Statuses: `200` refreshed (`refreshed`, or `replaced` when
//! a sign-in stored meanwhile was kept), `404` no sign-in stored, `409`
//! OpenAI refused the refresh token (sign in again), `422` the stored
//! sign-in cannot be used, `500` refreshed but not stored, `502` the refresh
//! could not be completed, `503` no broker.
use super::*;
use axocoatl_daemon::external_agent::codex_chatgpt::RefreshReport;

/// The status that goes with `report`.
pub(crate) fn refresh_status(report: &RefreshReport) -> StatusCode {
    match report {
        RefreshReport::Refreshed { .. } | RefreshReport::Replaced { .. } => StatusCode::OK,
        RefreshReport::NoSignIn { .. } => StatusCode::NOT_FOUND,
        RefreshReport::Refused { .. } => StatusCode::CONFLICT,
        RefreshReport::Unusable { .. } => StatusCode::UNPROCESSABLE_ENTITY,
        RefreshReport::NotStored { .. } => StatusCode::INTERNAL_SERVER_ERROR,
        RefreshReport::Failed { .. } => StatusCode::BAD_GATEWAY,
        RefreshReport::Unavailable { .. } => StatusCode::SERVICE_UNAVAILABLE,
    }
}

pub async fn refresh_codex_sign_in(
    State(state): State<AppState>,
) -> (StatusCode, Json<RefreshReport>) {
    let report = state.read().await.refresh_codex_chatgpt_sign_in().await;
    (refresh_status(&report), Json(report))
}

/// Refuse the request unless the daemon authenticates API requests
/// (`authenticated`) and the peer is on a loopback address. An IPv4 peer on
/// a dual-stack listener (`server.host: "[::]"`) arrives as an IPv4-mapped
/// IPv6 address (`::ffff:127.0.0.1`), so the address is canonicalized first.
pub async fn require_authenticated_loopback(
    authenticated: bool,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    if !authenticated {
        return err(
            StatusCode::FORBIDDEN,
            "this daemon accepts API requests without authentication, so it does not refresh \
             the ChatGPT sign-in through its API; restart it with API authentication (the \
             local API token, or server.auth credentials)",
        )
        .into_response();
    }
    let loopback = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .is_some_and(|peer| peer.0.ip().to_canonical().is_loopback());
    if !loopback {
        return err(
            StatusCode::FORBIDDEN,
            "the ChatGPT sign-in is refreshed only for a request from this computer (a \
             loopback address)",
        )
        .into_response();
    }
    next.run(request).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower::ServiceExt;

    /// The guard in front of a handler that only says it was reached.
    fn guarded(authenticated: bool) -> axum::Router {
        let route = axum::routing::post(|| async { "served" }).layer(axum::middleware::from_fn(
            move |request: axum::extract::Request, next: Next| {
                require_authenticated_loopback(authenticated, request, next)
            },
        ));
        axum::Router::new().route("/api/connect/codex/refresh", route)
    }

    async fn call(app: &axum::Router, peer: Option<SocketAddr>) -> (StatusCode, serde_json::Value) {
        let mut request = axum::http::Request::post("/api/connect/codex/refresh")
            .body(axum::body::Body::empty())
            .unwrap();
        if let Some(peer) = peer {
            request.extensions_mut().insert(ConnectInfo(peer));
        }
        let response = app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes)
                .unwrap_or_else(|_| String::from_utf8_lossy(&bytes).into_owned().into()),
        )
    }

    #[test]
    fn every_outcome_has_its_status() {
        let message = || "m".to_string();
        for (report, status) in [
            (
                RefreshReport::Refreshed {
                    refreshed_at_ms: 1,
                    expires_at_ms: Some(2),
                },
                StatusCode::OK,
            ),
            (
                RefreshReport::Replaced {
                    refreshed_at_ms: 1,
                    expires_at_ms: None,
                },
                StatusCode::OK,
            ),
            (
                RefreshReport::NotStored {
                    refreshed_at_ms: 1,
                    expires_at_ms: None,
                    message: message(),
                },
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
            (
                RefreshReport::NoSignIn { message: message() },
                StatusCode::NOT_FOUND,
            ),
            (
                RefreshReport::Unusable { message: message() },
                StatusCode::UNPROCESSABLE_ENTITY,
            ),
            (
                RefreshReport::Refused {
                    message: message(),
                    sent: true,
                },
                StatusCode::CONFLICT,
            ),
            (
                RefreshReport::Failed { message: message() },
                StatusCode::BAD_GATEWAY,
            ),
            (
                RefreshReport::Unavailable { message: message() },
                StatusCode::SERVICE_UNAVAILABLE,
            ),
        ] {
            assert_eq!(refresh_status(&report), status, "{report:?}");
            let json = serde_json::to_value(&report).unwrap();
            assert_eq!(json["outcome"], report.outcome(), "{json}");
        }
    }

    /// A peer that is not on loopback, no peer at all (another transport),
    /// and a daemon without API authentication are refused before the
    /// handler runs; a loopback peer of an authenticating daemon reaches it.
    #[tokio::test]
    async fn only_an_authenticated_loopback_caller_is_served() {
        let local = SocketAddr::from(([127, 0, 0, 1], 50000));
        // 127.0.0.1, ::1, and 127.0.0.1 as a dual-stack listener sees it.
        let mapped = SocketAddr::from((std::net::Ipv4Addr::LOCALHOST.to_ipv6_mapped(), 50000));
        for peer in [
            local,
            SocketAddr::from(([0, 0, 0, 0, 0, 0, 0, 1], 50000)),
            mapped,
        ] {
            let (status, body) = call(&guarded(true), Some(peer)).await;
            assert_eq!(status, StatusCode::OK, "{peer}");
            assert_eq!(body, "served");
        }
        let lan = std::net::Ipv4Addr::new(192, 168, 1, 20);
        for peer in [
            SocketAddr::from((lan, 50000)),
            SocketAddr::from((lan.to_ipv6_mapped(), 50000)),
        ] {
            let (status, body) = call(&guarded(true), Some(peer)).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{peer}");
            assert!(
                body["error"].as_str().unwrap().contains("loopback"),
                "{body}"
            );
        }
        let (status, _) = call(&guarded(true), None).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, body) = call(&guarded(false), Some(local)).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(
            body["error"]
                .as_str()
                .unwrap()
                .contains("without authentication"),
            "{body}"
        );
    }
}
