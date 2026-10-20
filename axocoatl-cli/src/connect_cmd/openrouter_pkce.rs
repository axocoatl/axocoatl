//! OpenRouter's OAuth PKCE sign-in for `axocoatl connect openrouter`
//! (<https://openrouter.ai/docs/use-cases/oauth-pkce>): this sign-in's
//! values ([`Pkce`]), the sign-in page's address ([`auth_url`]) and the
//! loopback server the browser returns to ([`Callback`]).
//!
//! The verifier is 32 bytes from the operating system's random source in
//! unpadded base64url (43 characters, RFC 7636 section 4.1), the challenge is
//! its SHA-256 in unpadded base64url (`S256`), and the state is 16 more
//! random bytes. The verifier never leaves this process except in the
//! exchange request to OpenRouter's API; the challenge and the state are in
//! the sign-in page's address, which is not secret. The browser returns to
//! `http://127.0.0.1:<port>/callback?code=…&state=…` on a port the operating
//! system chose: only a request for that path, with this computer's
//! address as its `Host`, is read as the sign-in's answer, and its state
//! must be this sign-in's (compared in constant time) before its code is
//! used. A code without the state, or with another state, is answered
//! `400`, counted ([`Callback::ignored`]) and never exchanged; the sign-in
//! goes on waiting, so a stray or forged request cannot end it. A return
//! without a code is a cancelled sign-in (OpenRouter returns no state when
//! access is denied) only when the browser navigated to it
//! (`Sec-Fetch-Mode: navigate`), or, from a client that sends no
//! `Sec-Fetch-Mode` at all, when it carries OpenRouter's `error`; any other
//! request without a code is answered and ignored.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use zeroize::Zeroizing;

/// OpenRouter's sign-in page.
pub(crate) const AUTH_PAGE: &str = "https://openrouter.ai/auth";
/// The path the browser returns to.
pub(crate) const CALLBACK_PATH: &str = "/callback";
/// The challenge method, in the page's address and in the exchange.
pub(crate) const CHALLENGE_METHOD: &str = "S256";
/// How long one browser connection may take to send its request.
const REQUEST_WAIT: Duration = Duration::from_secs(5);
/// The longest request read: a request line and headers.
const MAX_REQUEST_BYTES: usize = 16 * 1024;
/// The longest code accepted (OpenRouter's are far shorter).
const MAX_CODE_BYTES: usize = 2048;

fn base64url(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// One sign-in's PKCE values. It has no `Debug`: the verifier is secret.
pub(crate) struct Pkce {
    verifier: Zeroizing<String>,
    challenge: String,
    state: String,
}

impl Pkce {
    /// Fresh values from the operating system's random source.
    pub(crate) fn new() -> Result<Self, String> {
        let mut verifier = Zeroizing::new([0u8; 32]);
        let mut state = [0u8; 16];
        getrandom::fill(&mut *verifier)
            .and_then(|()| getrandom::fill(&mut state))
            .map_err(|error| format!("the operating system's random source failed: {error}"))?;
        Ok(Self::from_random(&verifier, &state))
    }

    /// The values made from these random bytes.
    pub(crate) fn from_random(verifier: &[u8; 32], state: &[u8; 16]) -> Self {
        let verifier = Zeroizing::new(base64url(verifier));
        Self {
            challenge: challenge_for(&verifier),
            verifier,
            state: base64url(state),
        }
    }

    /// The verifier, sent only in the exchange request.
    pub(crate) fn verifier(&self) -> &str {
        &self.verifier
    }

    /// The `S256` challenge, in the sign-in page's address.
    pub(crate) fn challenge(&self) -> &str {
        &self.challenge
    }

    /// The state, in the sign-in page's address and the browser's return.
    pub(crate) fn state(&self) -> &str {
        &self.state
    }

    /// Whether `returned` is this sign-in's state, compared in constant time.
    #[cfg(test)]
    pub(crate) fn state_matches(&self, returned: &str) -> bool {
        same_text(&self.state, returned)
    }
}

/// `a == b`, in time that depends only on their lengths.
fn same_text(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    a.len() == b.len()
        && a.iter()
            .zip(b)
            .fold(0u8, |difference, (x, y)| difference | (x ^ y))
            == 0
}

/// The `S256` challenge of `verifier`: its SHA-256 in unpadded base64url.
pub(crate) fn challenge_for(verifier: &str) -> String {
    base64url(&Sha256::digest(verifier.as_bytes()))
}

/// The sign-in page's address: `page` with the callback, the challenge,
/// the method, the state and a label for the new key.
pub(crate) fn auth_url(page: &str, callback: &str, pkce: &Pkce, key_label: &str) -> String {
    match reqwest::Url::parse_with_params(
        page,
        [
            ("callback_url", callback),
            ("code_challenge", pkce.challenge()),
            ("code_challenge_method", CHALLENGE_METHOD),
            ("state", pkce.state()),
            ("key_label", key_label),
        ],
    ) {
        Ok(url) => url.to_string(),
        // `page` is a constant, or a test server's address.
        Err(_) => page.to_string(),
    }
}

/// How the browser's return ended the sign-in.
pub(crate) enum Returned {
    /// The code to exchange, with this sign-in's state.
    Code(Zeroizing<String>),
    /// No code: the sign-in was cancelled or access was denied. The error
    /// OpenRouter named, when it is a short word that can be shown.
    Cancelled(Option<String>),
}

/// What one request to the loopback server was.
#[derive(Debug, PartialEq, Eq)]
enum Request {
    /// Not the sign-in's answer: answered and ignored.
    Other(u16, &'static str),
    Code(String),
    Cancelled(Option<String>),
    /// A code without this sign-in's state: answered `400`, counted and
    /// ignored.
    WrongState,
}

/// The loopback server the browser returns to.
pub(crate) struct Callback {
    listener: TcpListener,
    port: u16,
    ignored: Arc<AtomicUsize>,
}

impl Callback {
    /// Listen on `127.0.0.1`, on a port the operating system chooses.
    pub(crate) async fn bind() -> std::io::Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let port = listener.local_addr()?.port();
        Ok(Self {
            listener,
            port,
            ignored: Arc::default(),
        })
    }

    /// How many returns carried a code without this sign-in's state (each
    /// answered `400` and never exchanged).
    pub(crate) fn ignored(&self) -> usize {
        self.ignored.load(Ordering::SeqCst)
    }

    /// The port the system chose.
    pub(crate) fn port(&self) -> u16 {
        self.port
    }

    /// The address the browser returns to.
    pub(crate) fn url(&self) -> String {
        format!("http://127.0.0.1:{}{CALLBACK_PATH}", self.port)
    }

    /// Serve requests until one is this sign-in's answer: its code with
    /// this sign-in's state, or a cancellation. Each connection is read on
    /// its own task, so one that sends nothing (it is dropped after
    /// [`REQUEST_WAIT`]) holds up no other. A request for another path or
    /// host, a code with another state or none, and a request without a
    /// code that is not a cancellation are answered and ignored, and a
    /// connection that fails before it is accepted is skipped. The caller
    /// bounds the wait.
    pub(crate) async fn wait(&self, pkce: &Pkce) -> Returned {
        let (sender, mut receiver) = tokio::sync::mpsc::channel::<Request>(8);
        loop {
            tokio::select! {
                accepted = self.listener.accept() => {
                    let Ok((mut stream, _)) = accepted else {
                        // A connection reset before it was accepted, or no
                        // descriptor left for a moment: try again shortly.
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        continue;
                    };
                    let (sender, port, state) = (sender.clone(), self.port, pkce.state.clone());
                    tokio::spawn(async move {
                        let Ok(Some(request)) =
                            tokio::time::timeout(REQUEST_WAIT, read_request(&mut stream)).await
                        else {
                            return;
                        };
                        let request = classify(&request, port, &state);
                        let (status, page) = match &request {
                            Request::Other(status, page) => (*status, *page),
                            Request::Code(_) => (200, SIGNED_IN_PAGE),
                            Request::Cancelled(_) => (200, CANCELLED_PAGE),
                            Request::WrongState => (400, WRONG_STATE_PAGE),
                        };
                        respond(&mut stream, status, page).await;
                        let _ = sender.send(request).await;
                    });
                }
                Some(request) = receiver.recv() => match request {
                    Request::Other(..) => {}
                    Request::WrongState => {
                        self.ignored.fetch_add(1, Ordering::SeqCst);
                    }
                    Request::Code(code) => return Returned::Code(Zeroizing::new(code)),
                    Request::Cancelled(error) => return Returned::Cancelled(error),
                },
            }
        }
    }
}

/// A request's line and headers, read up to their blank line.
struct RawRequest {
    method: String,
    target: String,
    host: Option<String>,
    /// `Sec-Fetch-Mode`, which browsers send: `navigate` for a page the
    /// browser opens, as OpenRouter's redirect is.
    fetch_mode: Option<String>,
}

async fn read_request(stream: &mut TcpStream) -> Option<RawRequest> {
    let mut bytes = Zeroizing::new(Vec::with_capacity(1024));
    let mut buffer = [0u8; 1024];
    let end = loop {
        let read = stream.read(&mut buffer).await.ok()?;
        if read == 0 {
            return None;
        }
        bytes.extend_from_slice(&buffer[..read]);
        if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break end;
        }
        if bytes.len() > MAX_REQUEST_BYTES {
            return None;
        }
    };
    let head = std::str::from_utf8(&bytes[..end]).ok()?;
    let mut lines = head.split("\r\n");
    let mut request_line = lines.next()?.split(' ');
    let (method, target, version) = (
        request_line.next()?,
        request_line.next()?,
        request_line.next()?,
    );
    if request_line.next().is_some() || !version.starts_with("HTTP/1.") {
        return None;
    }
    let headers: Vec<(&str, &str)> = lines
        .filter_map(|line| {
            let (name, value) = line.split_once(':')?;
            Some((name.trim(), value.trim()))
        })
        .collect();
    let header = |wanted: &str| {
        headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(wanted))
            .map(|(_, value)| value.to_string())
    };
    Some(RawRequest {
        method: method.to_string(),
        target: target.to_string(),
        host: header("host"),
        fetch_mode: header("sec-fetch-mode"),
    })
}

/// A value of `name` in `pairs`, when there is exactly one.
fn single(pairs: &[(String, String)], name: &str) -> Result<Option<String>, ()> {
    let mut values = pairs.iter().filter(|(key, _)| key == name);
    match (values.next(), values.next()) {
        (None, _) => Ok(None),
        (Some((_, value)), None) => Ok(Some(value.clone())),
        (Some(_), Some(_)) => Err(()),
    }
}

/// A short word OpenRouter may name as the reason (`access_denied`), shown
/// only when it is one.
fn shown_error(error: Option<String>) -> Option<String> {
    error.filter(|error| {
        !error.is_empty()
            && error.len() <= 64
            && error
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_-.".contains(&byte))
    })
}

fn classify(request: &RawRequest, port: u16, expected_state: &str) -> Request {
    if request.method != "GET" {
        return Request::Other(405, NOT_HERE_PAGE);
    }
    let host_ok = request.host.as_deref().is_some_and(|host| {
        host.eq_ignore_ascii_case(&format!("127.0.0.1:{port}"))
            || host.eq_ignore_ascii_case(&format!("localhost:{port}"))
    });
    if !host_ok || !request.target.starts_with('/') {
        return Request::Other(400, NOT_HERE_PAGE);
    }
    let Ok(url) = reqwest::Url::parse(&format!("http://127.0.0.1:{port}{}", request.target)) else {
        return Request::Other(400, NOT_HERE_PAGE);
    };
    if url.path() != CALLBACK_PATH {
        return Request::Other(404, NOT_HERE_PAGE);
    }
    let pairs: Vec<(String, String)> = url
        .query_pairs()
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    let (Ok(code), Ok(state), Ok(error)) = (
        single(&pairs, "code"),
        single(&pairs, "state"),
        single(&pairs, "error"),
    ) else {
        return Request::WrongState;
    };
    let Some(code) = code.filter(|code| !code.is_empty()) else {
        // A browser navigating here (OpenRouter's redirect) without a code,
        // or a client that sends no `Sec-Fetch-Mode` naming OpenRouter's
        // error, cancels. A page's own request (`cors`, `no-cors`) or a
        // bare request never does.
        let cancels = match request.fetch_mode.as_deref() {
            Some(mode) => mode.eq_ignore_ascii_case("navigate"),
            None => error.is_some(),
        };
        return if cancels {
            Request::Cancelled(shown_error(error))
        } else {
            Request::Other(400, NOT_HERE_PAGE)
        };
    };
    let code_ok = code.len() <= MAX_CODE_BYTES && code.bytes().all(|byte| byte.is_ascii_graphic());
    match state {
        Some(state) if code_ok && same_text(expected_state, &state) => Request::Code(code),
        _ => Request::WrongState,
    }
}

async fn respond(stream: &mut TcpStream, status: u16, page: &str) {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        _ => "Method Not Allowed",
    };
    let body = format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <title>Axocoatl</title><style>body{{font:16px/1.5 system-ui,sans-serif;\
         max-width:36rem;margin:15vh auto;padding:0 1rem;color:#1b1b1b;background:#fff}}\
         @media (prefers-color-scheme:dark){{body{{color:#eee;background:#161616}}}}</style>\
         </head><body>{page}</body></html>"
    );
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\n\
         Content-Security-Policy: default-src 'none'; style-src 'unsafe-inline'\r\n\
         X-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}

const SIGNED_IN_PAGE: &str = "<h1>Signed in to OpenRouter</h1><p>Axocoatl received the \
     sign-in and is getting the key and checking it now. You can close this tab and go back to \
     the terminal.</p>";
const CANCELLED_PAGE: &str = "<h1>Sign-in cancelled</h1><p>OpenRouter returned no sign-in, so \
     Axocoatl stored nothing. You can close this tab; run <code>axocoatl connect \
     openrouter</code> again to retry.</p>";
const WRONG_STATE_PAGE: &str = "<h1>Not this sign-in</h1><p>This answer was not for the \
     sign-in the Axocoatl command waiting here started, so it was not used. That sign-in is \
     still waiting: finish it in the browser tab it opened, or run <code>axocoatl connect \
     openrouter</code> again.</p>";
const NOT_HERE_PAGE: &str = "<h1>Axocoatl</h1><p>Nothing here.</p>";

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// RFC 7636, appendix B: the verifier, its challenge.
    #[test]
    fn the_challenge_is_the_rfc_7636_s256_of_the_verifier() {
        assert_eq!(
            challenge_for("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        // The appendix's 32 octets give that verifier.
        let octets: [u8; 32] = [
            116, 24, 223, 180, 151, 153, 224, 37, 79, 250, 96, 125, 216, 173, 187, 186, 22, 212,
            37, 77, 105, 214, 191, 240, 91, 88, 5, 88, 83, 132, 141, 121,
        ];
        let pkce = Pkce::from_random(&octets, &[0; 16]);
        assert_eq!(
            pkce.verifier(),
            "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"
        );
        assert_eq!(
            pkce.challenge(),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn fresh_values_are_random_unreserved_and_of_rfc_length() {
        let first = Pkce::new().unwrap();
        let second = Pkce::new().unwrap();
        for pkce in [&first, &second] {
            // 43 to 128 unreserved characters (RFC 7636 section 4.1).
            assert_eq!(pkce.verifier().len(), 43);
            assert!(pkce
                .verifier()
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"-._~".contains(&byte)));
            assert_eq!(pkce.challenge(), challenge_for(pkce.verifier()));
            assert_eq!(pkce.state().len(), 22);
        }
        assert_ne!(first.verifier(), second.verifier());
        assert_ne!(first.state(), second.state());
        assert_ne!(first.challenge(), second.challenge());
    }

    #[test]
    fn only_this_sign_ins_state_matches() {
        let pkce = Pkce::from_random(&[7; 32], &[9; 16]);
        assert!(pkce.state_matches(pkce.state()));
        let mut other = pkce.state().to_string();
        other.replace_range(0..1, if other.starts_with('A') { "B" } else { "A" });
        assert!(!pkce.state_matches(&other));
        assert!(!pkce.state_matches(""));
        assert!(!pkce.state_matches(&pkce.state()[1..]));
        assert!(!pkce.state_matches(&format!("{}x", pkce.state())));
    }

    #[test]
    fn the_page_address_carries_the_callback_challenge_method_state_and_label_never_the_verifier() {
        let pkce = Pkce::from_random(&[1; 32], &[2; 16]);
        let url = auth_url(
            AUTH_PAGE,
            "http://127.0.0.1:51423/callback",
            &pkce,
            "Axocoatl 2026-10-19",
        );
        let parsed = reqwest::Url::parse(&url).unwrap();
        assert_eq!(
            parsed.origin().ascii_serialization(),
            "https://openrouter.ai"
        );
        assert_eq!(parsed.path(), "/auth");
        let pairs: Vec<(String, String)> = parsed
            .query_pairs()
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect();
        let value = |name: &str| {
            pairs
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.as_str())
        };
        assert_eq!(
            value("callback_url"),
            Some("http://127.0.0.1:51423/callback")
        );
        assert_eq!(value("code_challenge"), Some(pkce.challenge()));
        assert_eq!(value("code_challenge_method"), Some("S256"));
        assert_eq!(value("state"), Some(pkce.state()));
        assert_eq!(value("key_label"), Some("Axocoatl 2026-10-19"));
        assert!(!url.contains(pkce.verifier()));
        // The callback is encoded inside the query.
        assert!(url.contains("callback_url=http%3A%2F%2F127.0.0.1%3A51423%2Fcallback"));
    }

    fn raw(method: &str, target: &str, host: Option<&str>) -> RawRequest {
        RawRequest {
            method: method.into(),
            target: target.into(),
            host: host.map(str::to_string),
            fetch_mode: Some("navigate".into()),
        }
    }

    fn with_mode(mut request: RawRequest, mode: Option<&str>) -> RawRequest {
        request.fetch_mode = mode.map(str::to_string);
        request
    }

    #[test]
    fn a_request_is_the_answer_only_on_the_callback_path_and_host_with_this_state() {
        let pkce = Pkce::from_random(&[3; 32], &[4; 16]);
        let host = Some("127.0.0.1:4100");
        let state = pkce.state().to_string();
        let callback = |query: &str| raw("GET", &format!("/callback?{query}"), host);
        assert_eq!(
            classify(
                &callback(&format!("code=abc-123&state={state}")),
                4100,
                pkce.state()
            ),
            Request::Code("abc-123".into())
        );
        // `localhost` names the same server.
        assert_eq!(
            classify(
                &raw(
                    "GET",
                    &format!("/callback?state={state}&code=c"),
                    Some("LOCALHOST:4100")
                ),
                4100,
                pkce.state()
            ),
            Request::Code("c".into())
        );
        // Another state, none, or two: never exchanged, the sign-in waits on.
        for query in [
            "code=abc".to_string(),
            "code=abc&state=forged".to_string(),
            format!("code=abc&state={state}&state={state}"),
            format!("code=abc&code=def&state={state}"),
            format!("code=a%20b&state={state}"),
        ] {
            assert_eq!(
                classify(&callback(&query), 4100, pkce.state()),
                Request::WrongState,
                "{query}"
            );
        }
        // No code, when the browser navigated here: cancelled, with
        // OpenRouter's reason when it is a word.
        assert_eq!(
            classify(&callback("error=access_denied"), 4100, pkce.state()),
            Request::Cancelled(Some("access_denied".into()))
        );
        // From a client without Sec-Fetch-Mode, only with OpenRouter's error.
        assert_eq!(
            classify(
                &with_mode(callback("error=access_denied"), None),
                4100,
                pkce.state()
            ),
            Request::Cancelled(Some("access_denied".into()))
        );
        // A bare request (curl), or a page's own request: ignored.
        for request in [
            with_mode(raw("GET", "/callback", host), None),
            with_mode(raw("GET", "/callback", host), Some("no-cors")),
            with_mode(callback("error=access_denied"), Some("cors")),
        ] {
            assert_eq!(
                classify(&request, 4100, pkce.state()),
                Request::Other(400, NOT_HERE_PAGE),
                "{} {:?}",
                request.target,
                request.fetch_mode
            );
        }
        assert_eq!(
            classify(&callback("error=%3Cscript%3E"), 4100, pkce.state()),
            Request::Cancelled(None)
        );
        assert_eq!(
            classify(&raw("GET", "/callback", host), 4100, pkce.state()),
            Request::Cancelled(None)
        );
        assert_eq!(
            classify(
                &callback(&format!("code=&state={state}")),
                4100,
                pkce.state()
            ),
            Request::Cancelled(None)
        );
        // Anything else is answered and ignored.
        for (request, status) in [
            (raw("GET", "/favicon.ico", host), 404),
            (
                raw("POST", &format!("/callback?code=a&state={state}"), host),
                405,
            ),
            (
                raw(
                    "GET",
                    &format!("/callback?code=a&state={state}"),
                    Some("evil.example:4100"),
                ),
                400,
            ),
            (
                raw(
                    "GET",
                    &format!("/callback?code=a&state={state}"),
                    Some("127.0.0.1:4101"),
                ),
                400,
            ),
            (
                raw("GET", &format!("/callback?code=a&state={state}"), None),
                400,
            ),
            (raw("GET", "http://127.0.0.1:4100/callback", host), 400),
        ] {
            assert!(
                matches!(classify(&request, 4100, pkce.state()), Request::Other(s, _) if s == status),
                "{} {}",
                request.method,
                request.target
            );
        }
    }

    /// What a browser opening `url` gets: its status line and body.
    pub(crate) async fn browse(url: &str) -> (u16, String) {
        request(url, Some("navigate")).await
    }

    /// What a request to `url` with `Sec-Fetch-Mode: mode` (none: curl)
    /// gets.
    pub(crate) async fn request(url: &str, mode: Option<&str>) -> (u16, String) {
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let mut request = client.get(url);
        if let Some(mode) = mode {
            request = request.header("Sec-Fetch-Mode", mode);
        }
        let response = request.send().await.unwrap();
        let status = response.status().as_u16();
        (status, response.text().await.unwrap())
    }

    #[tokio::test]
    async fn the_server_ignores_other_requests_and_returns_the_code_with_this_state() {
        let callback = Callback::bind().await.unwrap();
        let url = callback.url();
        assert!(url.starts_with("http://127.0.0.1:"));
        assert!(url.ends_with("/callback"));
        let pkce = Pkce::from_random(&[5; 32], &[6; 16]);
        let state = pkce.state().to_string();
        let browser = tokio::spawn({
            let url = url.clone();
            async move {
                let origin = url.trim_end_matches(CALLBACK_PATH).to_string();
                // A slow connection that never sends a request is dropped.
                let silent = TcpStream::connect(origin.trim_start_matches("http://"))
                    .await
                    .unwrap();
                let not_here = browse(&format!("{origin}/favicon.ico")).await;
                let answer = browse(&format!("{url}?code=the-code&state={state}")).await;
                drop(silent);
                (not_here, answer)
            }
        });
        let returned = tokio::time::timeout(Duration::from_secs(30), callback.wait(&pkce))
            .await
            .unwrap();
        let ((not_here, _), (status, page)) = browser.await.unwrap();
        assert_eq!(not_here, 404);
        assert_eq!(status, 200);
        assert!(page.contains("Signed in to OpenRouter"), "{page}");
        match returned {
            Returned::Code(code) => assert_eq!(code.as_str(), "the-code"),
            _ => panic!("no code"),
        }
    }

    /// A forged or stale return, a bare request and a page's own request
    /// are answered and ignored: the sign-in still takes its real answer.
    #[tokio::test]
    async fn stray_and_forged_returns_do_not_end_the_sign_in() {
        let callback = Callback::bind().await.unwrap();
        let pkce = Pkce::from_random(&[8; 32], &[9; 16]);
        let url = callback.url();
        let state = pkce.state().to_string();
        let browser = tokio::spawn(async move {
            let forged = browse(&format!("{url}?code=x&state=forged")).await;
            let no_state = browse(&format!("{url}?code=x")).await;
            let bare = request(&url, None).await;
            let page = request(&format!("{url}?error=access_denied"), Some("no-cors")).await;
            let answer = browse(&format!("{url}?code=real&state={state}")).await;
            (forged, no_state, bare, page, answer)
        });
        let returned = tokio::time::timeout(Duration::from_secs(30), callback.wait(&pkce))
            .await
            .unwrap();
        let (forged, no_state, bare, page, answer) = browser.await.unwrap();
        for (status, body) in [&forged, &no_state] {
            assert_eq!(*status, 400);
            assert!(body.contains("Not this sign-in"), "{body}");
            assert!(body.contains("still waiting"), "{body}");
        }
        assert_eq!(bare.0, 400);
        assert_eq!(page.0, 400);
        assert_eq!(answer.0, 200);
        assert_eq!(callback.ignored(), 2);
        match returned {
            Returned::Code(code) => assert_eq!(code.as_str(), "real"),
            Returned::Cancelled(_) => panic!("cancelled"),
        }
    }

    /// The browser returning without a code ends the sign-in as cancelled.
    #[tokio::test]
    async fn a_return_without_a_code_cancels_the_sign_in() {
        for (mode, query) in [
            (Some("navigate"), "error=access_denied"),
            (Some("navigate"), ""),
            (None, "error=access_denied"),
        ] {
            let callback = Callback::bind().await.unwrap();
            let pkce = Pkce::from_random(&[8; 32], &[9; 16]);
            let url = format!("{}?{query}", callback.url());
            let browser = tokio::spawn(async move { request(&url, mode).await });
            let returned = callback.wait(&pkce).await;
            let (status, page) = browser.await.unwrap();
            assert_eq!(status, 200);
            assert!(page.contains("Sign-in cancelled"), "{page}");
            match returned {
                Returned::Cancelled(error) => assert_eq!(
                    error.as_deref(),
                    (!query.is_empty()).then_some("access_denied"),
                    "{mode:?} {query}"
                ),
                Returned::Code(_) => panic!("{query} gave a code"),
            }
        }
    }
}
