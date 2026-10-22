//! Where a browser may show a Session's app as a page.
//!
//! **In Safari, a Preview never runs top-level.** Safari's engine (WebKit on
//! macOS, iOS and iPadOS) lets a page on a `*.localhost` address that runs
//! top-level set a `Domain=localhost` cookie from script, and then sends it to
//! the workbench on `localhost` and to every Session's Preview
//! (operate/security.mdx, "Safari shares `Domain=localhost` cookies").
//! Embedded in the workbench's frame, Safari gives a Preview no cookies at
//! all. So where the request comes from Safari's engine on an Apple platform
//! ([`is_safari_engine_on_apple`]) and asks for a page of its own
//! ([`page_request`]: `Sec-Fetch-Dest: document`, or any other navigation that
//! is not a frame), [`refuse_top_level_preview`] answers with a short page that
//! says why ([`top_level_preview_refusal`]) and the app is never asked. That
//! holds whatever led there: a link, a redirect, Command-click, the context
//! menu, a window the Preview opened, or an address typed or pasted. The
//! Preview in the workbench's frame (`Sec-Fetch-Dest: iframe`) and every
//! request its page makes keep working, and other browsers are unchanged. The
//! workbench also offers no way to open one (static/ui/preview-embedding.js),
//! so this is what someone who tries anyway sees.
//!
//! **The legacy path answers only fetch and programs.**
//! `/api/sessions/{id}/proxy/{port}` serves a Session's app from the
//! workbench's own origin, behind its sign-in. Its responses carry
//! `Content-Security-Policy: sandbox` without `allow-same-origin`, so a page
//! there ran with an opaque origin and could not use the workbench's API, but
//! it was still a page of the app's at the workbench's address, and the app's
//! redirects and cookies reached the browser from there.
//! [`refuse_legacy_preview_page`] now refuses, in every browser, any request
//! for something the browser would show, run or embed (a page, frame, script,
//! style sheet, image, worker…): only `fetch()` and `XMLHttpRequest`
//! (`Sec-Fetch-Dest: empty`) and clients that send no Fetch Metadata reach the
//! app, and the proxy no longer forwards the app's `Set-Cookie` there.

use axum::{
    extract::{ConnectInfo, Request},
    http::{header, uri::Authority, HeaderMap, HeaderValue, Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use std::net::SocketAddr;
use std::str::FromStr;

/// True when `user_agent` is Safari's engine, WebKit, on an Apple platform:
///
/// - every browser on iPhone, iPad and iPod touch. Apple has them all use
///   WebKit and its cookie handling, Chrome (`CriOS/`), Firefox (`FxiOS/`) and
///   Edge (`EdgiOS/`) included. Those three tokens count on their own, because
///   on an iPad they come with a desktop `Macintosh` user agent;
/// - on a Mac, `AppleWebKit/` with no Chromium (`Chrome/`, `Chromium/`,
///   `Edg/`, `OPR/`) or Firefox token: Safari, Safari Technology Preview,
///   Playwright's WebKit, and apps that show pages with WebKit, whose user
///   agent leaves out `Version/` and `Safari/` (those are not required).
///
/// Chromium-based browsers and Firefox on a Mac, and WebKit on Linux (which
/// refuses a `Domain=localhost` cookie), are not. A Safari whose user agent
/// was changed to another browser's (Develop > User Agent) is taken for that
/// browser. static/ui/preview-embedding.js makes the same decision in the
/// page, from `navigator.vendor` first.
pub(crate) fn is_safari_engine_on_apple(user_agent: &str) -> bool {
    let has = |token: &str| user_agent.contains(token);
    let ios = ["iPhone", "iPad", "iPod", "CriOS/", "FxiOS/", "EdgiOS/"]
        .into_iter()
        .any(has);
    if ios {
        return true;
    }
    has("Macintosh")
        && has("AppleWebKit/")
        && !["Chrome/", "Chromium/", "Edg/", "OPR/", "Firefox/"]
            .into_iter()
            .any(has)
}

/// A request for a page of its own, in a tab or a window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PageRequest {
    /// Fetch Metadata says the response becomes a page of its own:
    /// `Sec-Fetch-Dest: document`, or `Sec-Fetch-Mode: navigate` for anything
    /// other than a frame.
    TopLevel,
    /// No Fetch Metadata at all (Safari before 16.4), on a request that asks
    /// for a page: `Upgrade-Insecure-Requests: 1`, or an `Accept` that lists
    /// HTML first. It may be for the workbench's frame or for a tab; nothing
    /// tells them apart, so it is taken as a page of its own.
    Unlabelled,
}

fn header_text(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.trim().to_ascii_lowercase())
}

/// Whether this request asks for a page of its own ([`PageRequest`]), or
/// `None` for a frame (`Sec-Fetch-Dest: iframe` or `frame`), a subresource,
/// `fetch()`, a WebSocket, or a client that does not ask for a page.
pub(crate) fn page_request(headers: &HeaderMap) -> Option<PageRequest> {
    let dest = header_text(headers, "sec-fetch-dest");
    let mode = header_text(headers, "sec-fetch-mode");
    match (dest.as_deref(), mode.as_deref()) {
        (Some("document"), _) => Some(PageRequest::TopLevel),
        (Some("iframe" | "frame"), _) => None,
        (_, Some("navigate")) => Some(PageRequest::TopLevel),
        (Some(_), _) | (None, Some(_)) => None,
        (None, None) => {
            let upgrades =
                header_text(headers, "upgrade-insecure-requests").as_deref() == Some("1");
            let html_first = header_text(headers, header::ACCEPT.as_str()).is_some_and(|accept| {
                let first = accept.split(',').next().unwrap_or_default();
                let media = first.split(';').next().unwrap_or_default().trim();
                matches!(media, "text/html" | "application/xhtml+xml")
            });
            (upgrades || html_first).then_some(PageRequest::Unlabelled)
        }
    }
}

/// The page request ([`page_request`]) of Safari's engine on an Apple
/// platform ([`is_safari_engine_on_apple`]), which a Preview origin refuses.
pub(crate) fn top_level_preview_in_safari(headers: &HeaderMap) -> Option<PageRequest> {
    let user_agent = headers
        .get(header::USER_AGENT)
        .and_then(|value| value.to_str().ok())?;
    if !is_safari_engine_on_apple(user_agent) {
        return None;
    }
    page_request(headers)
}

/// A Session id as [`crate::routes`] validates it from a Preview Host
/// (`ses-` and lowercase letters, digits and `-`), or `None`.
fn checked_session_id(session_id: &str) -> Option<&str> {
    (session_id.starts_with("ses-")
        && session_id.len() <= 63
        && session_id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'))
    .then_some(session_id)
}

/// The workbench's own address for this Session: `localhost` on the port the
/// Preview's Host names, which is the daemon's listener.
fn workbench_url(session_id: &str, listener_port: Option<u16>) -> String {
    let host = listener_port
        .map(|port| format!("localhost:{port}"))
        .unwrap_or_else(|| "localhost".to_string());
    match checked_session_id(session_id) {
        Some(session_id) => format!("http://{host}/?session={session_id}"),
        None => format!("http://{host}/"),
    }
}

const REFUSAL_HEAD: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="referrer" content="no-referrer">
<title>Preview opens in the workbench</title>
<style>
:root { color-scheme: light dark; }
body { font: 16px/1.5 system-ui, sans-serif; max-width: 36rem; margin: 12vh auto; padding: 0 16px; }
code { font: 14px ui-monospace, SFMono-Regular, Menlo, monospace; }
</style>
</head>
<body>
"#;

/// What Safari gets for a Preview page it would show top-level: status 403,
/// a page that says why and links to the workbench, and nothing of the app.
/// It runs no script and loads nothing (`default-src 'none'`), and its CSP
/// `sandbox` gives it an opaque origin, so it can set no cookie either. A
/// `HEAD` request gets the same headers and no body.
pub(crate) fn top_level_preview_refusal(
    method: &Method,
    session_id: &str,
    listener_port: Option<u16>,
    kind: PageRequest,
) -> Response {
    let workbench = workbench_url(session_id, listener_port);
    let reason = match kind {
        PageRequest::TopLevel => {
            "<h1>This Preview opens only in the workbench</h1>\n\
<p>In Safari, Axocoatl does not show a Preview in a tab or window of its own. \
A page on a <code>*.localhost</code> address that runs in its own tab can set cookies \
that Safari then sends to the workbench and to every Session's Preview. In the \
workbench's Preview pane Safari gives it no cookies at all, so in Safari a Preview \
opens only there.</p>\n"
        }
        PageRequest::Unlabelled => {
            "<h1>This Safari cannot show Previews</h1>\n\
<p>Safari 16.4 and later say whether a page is for the workbench's Preview pane or \
for a tab of its own; this one did not. A Preview in a tab of its own can set cookies \
that Safari then sends to the workbench and to every Session's Preview, so Axocoatl \
shows no Preview in this Safari. Update Safari.</p>\n"
        }
    };
    let body = format!(
        "{REFUSAL_HEAD}{reason}<p><a href=\"{workbench}\">Open the workbench</a>. \
For a full-page Preview, open the workbench in a browser other than Safari on a Mac.</p>\n\
</body>\n</html>\n"
    );
    let mut response = if *method == Method::HEAD {
        StatusCode::FORBIDDEN.into_response()
    } else {
        (StatusCode::FORBIDDEN, body).into_response()
    };
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'none'; style-src 'unsafe-inline'; base-uri 'none'; form-action 'none'; sandbox",
        ),
    );
    response
}

/// Middleware in front of the Preview Host boundary
/// ([`crate::routes::preview_host_boundary`]): on a valid Preview Host, from
/// a loopback peer, a page request of Safari's engine on an Apple platform
/// ([`top_level_preview_in_safari`]) gets [`top_level_preview_refusal`].
/// Everything else, including an invalid Preview Host and a peer that is not
/// on loopback (the boundary refuses both), goes on unchanged.
pub async fn refuse_top_level_preview(request: Request, next: Next) -> Response {
    let Ok(Some(target)) = crate::routes::preview_target_from_headers(request.headers()) else {
        return next.run(request).await;
    };
    if request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .is_some_and(|peer| !peer.0.ip().is_loopback())
    {
        return next.run(request).await;
    }
    let Some(kind) = top_level_preview_in_safari(request.headers()) else {
        return next.run(request).await;
    };
    let listener_port = request
        .headers()
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| Authority::from_str(value).ok())
        .and_then(|authority| authority.port_u16());
    top_level_preview_refusal(request.method(), &target.session_id, listener_port, kind)
}

/// Whether a browser asked the legacy Preview path for anything other than
/// `fetch()` or `XMLHttpRequest`: a page, a frame or any other navigation
/// (`Sec-Fetch-Mode: navigate`), or a script, style sheet, image, worker or
/// other subresource (any `Sec-Fetch-Dest` but `empty`). A client that sends
/// no Fetch Metadata (a program, or a browser too old to send it) is not
/// refused; there the response's CSP `sandbox` still applies.
pub(crate) fn legacy_preview_request_is_refused(headers: &HeaderMap) -> bool {
    let dest = header_text(headers, "sec-fetch-dest");
    let mode = header_text(headers, "sec-fetch-mode");
    mode.as_deref() == Some("navigate") || dest.is_some_and(|dest| dest != "empty")
}

const LEGACY_REFUSAL: &str = "The legacy Preview path serves a Session's app only to \
fetch() and XMLHttpRequest, and to programs that call the Axocoatl API. It is the \
workbench's own origin, so a browser cannot open it as a page or frame, or load a \
script, style sheet, image or anything else from it. The workbench shows a Preview at \
its own address, <session>-p<port>.localhost.\n";

/// Route middleware of `/api/sessions/{id}/proxy/{port}[/…]`: a request
/// [`legacy_preview_request_is_refused`] refuses gets status 403 and a line of
/// text, in every browser; the app is never asked.
pub async fn refuse_legacy_preview_page(request: Request, next: Next) -> Response {
    if !legacy_preview_request_is_refused(request.headers()) {
        return next.run(request).await;
    }
    let mut response = if *request.method() == Method::HEAD {
        StatusCode::FORBIDDEN.into_response()
    } else {
        (StatusCode::FORBIDDEN, LEGACY_REFUSAL).into_response()
    };
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("sandbox; default-src 'none'"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, routing::any, Router};
    use tower::ServiceExt;

    const SAFARI_MAC: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/26.0 Safari/605.1.15";
    // Playwright's WebKit on macOS, as measured with Playwright 1.64.1.
    const PLAYWRIGHT_WEBKIT_MAC: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/27.2 Safari/605.1.15";
    const WKWEBVIEW_MAC: &str =
        "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko)";
    const SAFARI_IPHONE: &str = "Mozilla/5.0 (iPhone; CPU iPhone OS 18_5 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.5 Mobile/15E148 Safari/604.1";
    const SAFARI_IPAD: &str = "Mozilla/5.0 (iPad; CPU OS 18_5 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.5 Mobile/15E148 Safari/604.1";
    const CHROME_IPHONE: &str = "Mozilla/5.0 (iPhone; CPU iPhone OS 18_5 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) CriOS/138.0.7204.156 Mobile/15E148 Safari/604.1";
    const FIREFOX_IPHONE: &str = "Mozilla/5.0 (iPhone; CPU iPhone OS 18_5 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) FxiOS/141.0 Mobile/15E148 Safari/605.1.15";
    const EDGE_IPHONE: &str = "Mozilla/5.0 (iPhone; CPU iPhone OS 18_5 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.0 EdgiOS/138.3351.83 Mobile/15E148 Safari/605.1.15";
    // An iPad asking for desktop sites says Macintosh.
    const CHROME_IPAD_DESKTOP: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) CriOS/138.0.7204.156 Safari/604.1";
    const CHROME_MAC: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/141.0.0.0 Safari/537.36";
    // Playwright's Chromium on macOS, as measured with Playwright 1.64.1.
    const HEADLESS_CHROME_MAC: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) HeadlessChrome/156.0.8078.4 Safari/537.36";
    const EDGE_MAC: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/141.0.0.0 Safari/537.36 Edg/141.0.0.0";
    const OPERA_MAC: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36 OPR/124.0.0.0";
    const FIREFOX_MAC: &str =
        "Mozilla/5.0 (Macintosh; Intel Mac OS X 10.15; rv:143.0) Gecko/20100101 Firefox/143.0";
    const WEBKIT_LINUX: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/27.2 Safari/605.1.15";
    const CHROME_ANDROID: &str = "Mozilla/5.0 (Linux; Android 14; Pixel 7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/141.0.0.0 Mobile Safari/537.36";
    const CHROME_WINDOWS: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/141.0.0.0 Safari/537.36";

    const PREVIEW_HOST: &str = "ses-123e4567-e89b-12d3-a456-426614174000-p3000.localhost:18080";
    const NAVIGATION_ACCEPT: &str =
        "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8";

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.append(
                axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        headers
    }

    #[test]
    fn safari_engine_on_apple_is_every_ios_browser_and_webkit_on_a_mac() {
        for user_agent in [
            SAFARI_MAC,
            PLAYWRIGHT_WEBKIT_MAC,
            WKWEBVIEW_MAC,
            SAFARI_IPHONE,
            SAFARI_IPAD,
            CHROME_IPHONE,
            FIREFOX_IPHONE,
            EDGE_IPHONE,
            CHROME_IPAD_DESKTOP,
        ] {
            assert!(is_safari_engine_on_apple(user_agent), "{user_agent}");
        }
        for user_agent in [
            CHROME_MAC,
            HEADLESS_CHROME_MAC,
            EDGE_MAC,
            OPERA_MAC,
            FIREFOX_MAC,
            WEBKIT_LINUX,
            CHROME_ANDROID,
            CHROME_WINDOWS,
            "curl/8.7.1",
            "",
        ] {
            assert!(!is_safari_engine_on_apple(user_agent), "{user_agent}");
        }
    }

    #[test]
    fn a_page_request_is_a_document_or_a_navigation_that_is_not_a_frame() {
        // Safari 16.4 and later, as measured in Playwright WebKit 27.2: a
        // typed address, a link, a window, Command-click and a redirect are
        // `document`; the workbench's frame is `iframe`.
        let top = |dest: &str, mode: &str| {
            page_request(&headers(&[
                ("sec-fetch-dest", dest),
                ("sec-fetch-mode", mode),
                ("sec-fetch-site", "none"),
            ]))
        };
        assert_eq!(top("document", "navigate"), Some(PageRequest::TopLevel));
        assert_eq!(top("Document", "Navigate"), Some(PageRequest::TopLevel));
        assert_eq!(top("iframe", "navigate"), None);
        assert_eq!(top("frame", "navigate"), None);
        // Other navigations are not the workbench's frame.
        for dest in ["embed", "object", "fencedframe", "empty"] {
            assert_eq!(top(dest, "navigate"), Some(PageRequest::TopLevel), "{dest}");
        }
        assert_eq!(
            page_request(&headers(&[("sec-fetch-mode", "navigate")])),
            Some(PageRequest::TopLevel)
        );
        // Subresources, fetch() and WebSockets of a Preview page.
        for (dest, mode) in [
            ("script", "no-cors"),
            ("image", "no-cors"),
            ("style", "no-cors"),
            ("empty", "cors"),
            ("empty", "same-origin"),
            ("websocket", "websocket"),
            ("worker", "same-origin"),
        ] {
            assert_eq!(top(dest, mode), None, "{dest} {mode}");
        }
        assert_eq!(
            page_request(&headers(&[("sec-fetch-dest", "document")])),
            Some(PageRequest::TopLevel)
        );
        // Fetch Metadata without a destination or navigation.
        assert_eq!(page_request(&headers(&[("sec-fetch-mode", "cors")])), None);

        // No Fetch Metadata (Safari before 16.4): what asks for a page is
        // taken as a page of its own, wherever it is shown.
        assert_eq!(
            page_request(&headers(&[
                ("accept", NAVIGATION_ACCEPT),
                ("upgrade-insecure-requests", "1"),
            ])),
            Some(PageRequest::Unlabelled)
        );
        assert_eq!(
            page_request(&headers(&[("upgrade-insecure-requests", "1")])),
            Some(PageRequest::Unlabelled)
        );
        assert_eq!(
            page_request(&headers(&[("accept", "application/xhtml+xml, */*")])),
            Some(PageRequest::Unlabelled)
        );
        for accept in [
            "*/*",
            "image/webp,image/*;q=0.8,*/*;q=0.5",
            "application/json",
        ] {
            assert_eq!(
                page_request(&headers(&[("accept", accept)])),
                None,
                "{accept}"
            );
        }
        assert_eq!(page_request(&HeaderMap::new()), None);
    }

    #[test]
    fn only_safari_on_apple_has_its_page_requests_refused() {
        let navigation = |user_agent: &str, dest: &str| {
            top_level_preview_in_safari(&headers(&[
                ("user-agent", user_agent),
                ("sec-fetch-dest", dest),
                ("sec-fetch-mode", "navigate"),
            ]))
        };
        for user_agent in [SAFARI_MAC, SAFARI_IPHONE, CHROME_IPHONE, WKWEBVIEW_MAC] {
            assert_eq!(
                navigation(user_agent, "document"),
                Some(PageRequest::TopLevel),
                "{user_agent}"
            );
            assert_eq!(navigation(user_agent, "iframe"), None, "{user_agent}");
        }
        for user_agent in [CHROME_MAC, FIREFOX_MAC, WEBKIT_LINUX, EDGE_MAC] {
            assert_eq!(navigation(user_agent, "document"), None, "{user_agent}");
        }
        assert_eq!(
            top_level_preview_in_safari(&headers(&[
                ("sec-fetch-dest", "document"),
                ("sec-fetch-mode", "navigate"),
            ])),
            None,
            "a request without a User-Agent is not Safari"
        );
        assert_eq!(
            top_level_preview_in_safari(&headers(&[
                ("user-agent", SAFARI_MAC),
                ("accept", NAVIGATION_ACCEPT),
                ("upgrade-insecure-requests", "1"),
            ])),
            Some(PageRequest::Unlabelled)
        );
    }

    /// What the middleware answers, through a router whose app would answer
    /// every request with `app content`.
    async fn through_preview_guard(
        request: axum::http::Request<Body>,
    ) -> (StatusCode, HeaderMap, String) {
        let router = Router::new()
            .fallback(any(|| async { "app content" }))
            .layer(axum::middleware::from_fn(refuse_top_level_preview));
        let response = router.oneshot(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let body = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        (status, headers, String::from_utf8(body.to_vec()).unwrap())
    }

    fn preview_request(host: &str, pairs: &[(&str, &str)]) -> axum::http::Request<Body> {
        let mut builder = axum::http::Request::builder()
            .method(Method::GET)
            .uri("/dashboard?tab=1")
            .header(header::HOST, host);
        for (name, value) in pairs {
            builder = builder.header(*name, *value);
        }
        builder.body(Body::empty()).unwrap()
    }

    #[tokio::test]
    async fn safari_gets_the_refusal_page_for_a_top_level_preview_and_the_app_for_its_frame() {
        let document = [
            ("user-agent", SAFARI_MAC),
            ("sec-fetch-dest", "document"),
            ("sec-fetch-mode", "navigate"),
            ("sec-fetch-site", "none"),
            ("accept", NAVIGATION_ACCEPT),
        ];
        let (status, headers, body) =
            through_preview_guard(preview_request(PREVIEW_HOST, &document)).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(!body.contains("app content"), "{body}");
        assert!(
            body.contains("<h1>This Preview opens only in the workbench</h1>"),
            "{body}"
        );
        assert!(
            body.contains(
                "In Safari, Axocoatl does not show a Preview in a tab or window of its own."
            ),
            "{body}"
        );
        assert!(
            body.contains(r#"<a href="http://localhost:18080/?session=ses-123e4567-e89b-12d3-a456-426614174000">Open the workbench</a>"#),
            "{body}"
        );
        assert!(!body.contains("<script"), "{body}");
        let header = |name: header::HeaderName| {
            headers
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(str::to_string)
        };
        assert_eq!(
            header(header::CONTENT_TYPE).as_deref(),
            Some("text/html; charset=utf-8")
        );
        assert_eq!(header(header::CACHE_CONTROL).as_deref(), Some("no-store"));
        assert_eq!(
            header(header::X_CONTENT_TYPE_OPTIONS).as_deref(),
            Some("nosniff")
        );
        assert_eq!(
            header(header::REFERRER_POLICY).as_deref(),
            Some("no-referrer")
        );
        let csp = header(header::CONTENT_SECURITY_POLICY).unwrap();
        assert!(csp.starts_with("default-src 'none';"), "{csp}");
        assert!(csp.ends_with("; sandbox"), "{csp}");
        assert!(!csp.contains("script-src"), "{csp}");
        assert!(headers.get(header::SET_COOKIE).is_none());

        // The same Safari's frame, subresources, fetch() and WebSocket
        // upgrade reach the app.
        for (dest, mode) in [
            ("iframe", "navigate"),
            ("script", "no-cors"),
            ("empty", "cors"),
            ("websocket", "websocket"),
        ] {
            let (status, _, body) = through_preview_guard(preview_request(
                PREVIEW_HOST,
                &[
                    ("user-agent", SAFARI_MAC),
                    ("sec-fetch-dest", dest),
                    ("sec-fetch-mode", mode),
                ],
            ))
            .await;
            assert_eq!(
                (status, body.as_str()),
                (StatusCode::OK, "app content"),
                "{dest}"
            );
        }

        // Chromium, Firefox and WebKit on Linux get the app top-level.
        for user_agent in [HEADLESS_CHROME_MAC, FIREFOX_MAC, WEBKIT_LINUX] {
            let mut pairs = document;
            pairs[0] = ("user-agent", user_agent);
            let (status, _, body) =
                through_preview_guard(preview_request(PREVIEW_HOST, &pairs)).await;
            assert_eq!(
                (status, body.as_str()),
                (StatusCode::OK, "app content"),
                "{user_agent}"
            );
        }

        // A POST navigation (a form a Preview submits to a new window) is
        // refused too, and HEAD gets the headers without a body.
        let mut post = preview_request(PREVIEW_HOST, &document);
        *post.method_mut() = Method::POST;
        let (status, _, body) = through_preview_guard(post).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(!body.contains("app content"));
        let mut head = preview_request(PREVIEW_HOST, &document);
        *head.method_mut() = Method::HEAD;
        let (status, headers, body) = through_preview_guard(head).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body, "");
        assert!(headers.contains_key(header::CONTENT_SECURITY_POLICY));
    }

    #[tokio::test]
    async fn a_safari_without_fetch_metadata_gets_the_refusal_page_for_any_preview_page() {
        let (status, _, body) = through_preview_guard(preview_request(
            PREVIEW_HOST,
            &[
                ("user-agent", SAFARI_MAC),
                ("accept", NAVIGATION_ACCEPT),
                ("upgrade-insecure-requests", "1"),
            ],
        ))
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(
            body.contains("<h1>This Safari cannot show Previews</h1>"),
            "{body}"
        );
        assert!(body.contains("Safari 16.4 and later"), "{body}");
        // Its subresources still reach the app.
        let (status, _, body) = through_preview_guard(preview_request(
            PREVIEW_HOST,
            &[("user-agent", SAFARI_MAC), ("accept", "*/*")],
        ))
        .await;
        assert_eq!((status, body.as_str()), (StatusCode::OK, "app content"));
    }

    #[tokio::test]
    async fn the_guard_leaves_the_workbench_and_invalid_preview_hosts_alone() {
        let document = [
            ("user-agent", SAFARI_MAC),
            ("sec-fetch-dest", "document"),
            ("sec-fetch-mode", "navigate"),
        ];
        // The workbench itself, at localhost and at a loopback IP.
        for host in ["localhost:18080", "127.0.0.1:18080"] {
            let (status, _, body) = through_preview_guard(preview_request(host, &document)).await;
            assert_eq!(
                (status, body.as_str()),
                (StatusCode::OK, "app content"),
                "{host}"
            );
        }
        // An invalid Preview Host is the boundary's to refuse (421).
        let (status, _, body) =
            through_preview_guard(preview_request("attacker.localhost:18080", &document)).await;
        assert_eq!((status, body.as_str()), (StatusCode::OK, "app content"));
        // A peer that is not on loopback is the boundary's to refuse (403).
        let mut remote = preview_request(PREVIEW_HOST, &document);
        remote
            .extensions_mut()
            .insert(ConnectInfo(SocketAddr::from(([192, 0, 2, 10], 50000))));
        let (status, _, body) = through_preview_guard(remote).await;
        assert_eq!((status, body.as_str()), (StatusCode::OK, "app content"));
        let mut local = preview_request(PREVIEW_HOST, &document);
        local
            .extensions_mut()
            .insert(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 50000))));
        let (status, _, _) = through_preview_guard(local).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[test]
    fn the_refusal_links_only_to_the_workbench_on_the_listener_port() {
        assert_eq!(
            workbench_url("ses-abc-123", Some(18080)),
            "http://localhost:18080/?session=ses-abc-123"
        );
        assert_eq!(
            workbench_url("ses-abc", None),
            "http://localhost/?session=ses-abc"
        );
        // Nothing but a validated Session id reaches the page.
        for invalid in [
            "ses-<script>",
            "abc",
            "ses-ABC",
            "ses-a\"b",
            &format!("ses-{}", "a".repeat(60)),
        ] {
            assert_eq!(
                workbench_url(invalid, Some(1)),
                "http://localhost:1/",
                "{invalid}"
            );
        }
    }

    #[test]
    fn the_legacy_path_refuses_every_browser_request_but_fetch() {
        let refused = |dest: &str, mode: &str| {
            legacy_preview_request_is_refused(&headers(&[
                ("sec-fetch-dest", dest),
                ("sec-fetch-mode", mode),
            ]))
        };
        for (dest, mode) in [
            ("document", "navigate"),
            ("iframe", "navigate"),
            ("frame", "navigate"),
            ("embed", "navigate"),
            ("object", "no-cors"),
            ("script", "no-cors"),
            ("style", "no-cors"),
            ("image", "no-cors"),
            ("worker", "same-origin"),
            ("serviceworker", "same-origin"),
            ("sharedworker", "same-origin"),
            ("websocket", "websocket"),
            ("empty", "navigate"),
        ] {
            assert!(refused(dest, mode), "{dest} {mode}");
        }
        assert!(!refused("empty", "cors"));
        assert!(!refused("empty", "same-origin"));
        assert!(!refused("empty", "no-cors"));
        assert!(legacy_preview_request_is_refused(&headers(&[(
            "sec-fetch-mode",
            "navigate"
        )])));
        // Programs (and browsers too old for Fetch Metadata) send none.
        assert!(!legacy_preview_request_is_refused(&HeaderMap::new()));
        assert!(!legacy_preview_request_is_refused(&headers(&[
            ("authorization", "Bearer token"),
            ("accept", NAVIGATION_ACCEPT),
        ])));
    }

    #[tokio::test]
    async fn the_legacy_route_answers_a_page_request_with_text_in_every_browser() {
        let router = Router::new().route(
            "/api/sessions/{id}/proxy/{port}/{*tail}",
            any(|| async { "app content" })
                .layer(axum::middleware::from_fn(refuse_legacy_preview_page)),
        );
        let send = |pairs: &[(&str, &str)]| {
            let mut builder = axum::http::Request::builder()
                .uri("/api/sessions/ses-a/proxy/3000/index.html")
                .header(header::HOST, "localhost:18080");
            for (name, value) in pairs {
                builder = builder.header(*name, *value);
            }
            let router = router.clone();
            let request = builder.body(Body::empty()).unwrap();
            async move {
                let response = router.oneshot(request).await.unwrap();
                let status = response.status();
                let headers = response.headers().clone();
                let body = axum::body::to_bytes(response.into_body(), 1 << 20)
                    .await
                    .unwrap();
                (status, headers, String::from_utf8(body.to_vec()).unwrap())
            }
        };
        for user_agent in [SAFARI_MAC, HEADLESS_CHROME_MAC, FIREFOX_MAC, WEBKIT_LINUX] {
            for (dest, mode) in [
                ("document", "navigate"),
                ("iframe", "navigate"),
                ("script", "no-cors"),
                ("image", "no-cors"),
            ] {
                let (status, headers, body) = send(&[
                    ("user-agent", user_agent),
                    ("sec-fetch-dest", dest),
                    ("sec-fetch-mode", mode),
                    ("sec-fetch-site", "same-origin"),
                ])
                .await;
                assert_eq!(status, StatusCode::FORBIDDEN, "{user_agent} {dest}");
                assert!(!body.contains("app content"), "{user_agent} {dest}");
                assert!(
                    body.starts_with(
                        "The legacy Preview path serves a Session's app only to fetch()"
                    ),
                    "{body}"
                );
                assert_eq!(
                    headers
                        .get(header::CONTENT_TYPE)
                        .and_then(|value| value.to_str().ok()),
                    Some("text/plain; charset=utf-8")
                );
                assert_eq!(
                    headers
                        .get(header::CONTENT_SECURITY_POLICY)
                        .and_then(|value| value.to_str().ok()),
                    Some("sandbox; default-src 'none'")
                );
            }
            let (status, _, body) = send(&[
                ("user-agent", user_agent),
                ("sec-fetch-dest", "empty"),
                ("sec-fetch-mode", "cors"),
            ])
            .await;
            assert_eq!(
                (status, body.as_str()),
                (StatusCode::OK, "app content"),
                "{user_agent}"
            );
        }
        let (status, _, body) = send(&[("authorization", "Bearer token")]).await;
        assert_eq!((status, body.as_str()), (StatusCode::OK, "app content"));
    }
}
