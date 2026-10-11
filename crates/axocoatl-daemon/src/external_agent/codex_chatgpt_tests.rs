//! The ChatGPT sign-in: what `axocoatl connect codex` reads from Codex's
//! `auth.json`, how it is stored, and the daemon's broker against a fake
//! OpenAI token endpoint (refresh before expiry, rotation, `401`, refusals)
//! and behind a Session's route to a fake ChatGPT backend. Nothing here
//! reaches OpenAI.

use super::*;
use std::os::unix::fs::PermissionsExt;
use wiremock::matchers::{body_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// An unsigned JWT with `claims` (the broker reads claims; OpenAI checks
/// signatures).
pub(crate) fn jwt(claims: serde_json::Value) -> String {
    let encode = |value: &serde_json::Value| {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(value.to_string())
    };
    format!(
        "{}.{}.{}",
        encode(&serde_json::json!({"alg": "RS256", "typ": "JWT"})),
        encode(&claims),
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(uuid::Uuid::new_v4().as_bytes())
    )
}

/// An access token for `account` that expires `in_seconds` from now (a
/// fresh random `jti` makes each one different).
pub(crate) fn access_token(account: &str, in_seconds: i64) -> String {
    let exp = (now_ms() / 1000) as i64 + in_seconds;
    jwt(serde_json::json!({
        "exp": exp,
        "jti": uuid::Uuid::new_v4().to_string(),
        "https://api.openai.com/auth": {"chatgpt_account_id": account},
    }))
}

pub(crate) fn refresh_token() -> String {
    format!("rt_{}", uuid::Uuid::new_v4().simple())
}

/// `auth.json` as Codex 0.160.1 writes it after a browser sign-in.
pub(crate) fn codex_auth_json(access: &str, refresh: &str, account: &str) -> String {
    let id_token = jwt(serde_json::json!({
        "email": "someone@example.invalid",
        "https://api.openai.com/auth": {
            "chatgpt_account_id": account,
            "chatgpt_plan_type": "plus",
        },
    }));
    serde_json::to_string_pretty(&serde_json::json!({
        "auth_mode": "chatgpt",
        "OPENAI_API_KEY": format!("sk-proj-{}", uuid::Uuid::new_v4().simple()),
        "tokens": {
            "id_token": id_token,
            "access_token": access,
            "refresh_token": refresh,
            "account_id": account,
        },
        "last_refresh": "2026-10-11T12:00:00Z",
    }))
    .unwrap()
}

/// A private data root, as the daemon opens it.
fn data_root() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    dir
}

fn store(root: &Path, sign_in: &ChatgptSignIn) {
    crate::secret_store::set_secret(root, CODEX_CHATGPT_SECRET, &sign_in.encode().unwrap())
        .unwrap();
}

fn stored(root: &Path) -> ChatgptSignIn {
    let bytes =
        std::fs::read(crate::secret_store::secret_path(root, CODEX_CHATGPT_SECRET)).unwrap();
    ChatgptSignIn::decode(&bytes).unwrap()
}

fn sign_in(access: &str, refresh: &str, account: &str) -> ChatgptSignIn {
    ChatgptSignIn::new(
        Zeroizing::new(access.into()),
        Zeroizing::new(refresh.into()),
        Some(account.into()),
        now_ms(),
    )
    .unwrap()
}

#[test]
fn codex_auth_json_is_read_exactly_and_stored_in_one_line() {
    let (access, refresh) = (access_token("acct-123", 3600), refresh_token());
    let text = codex_auth_json(&access, &refresh, "acct-123");
    let read = ChatgptSignIn::from_codex_auth_json(text.as_bytes(), 42).unwrap();
    assert_eq!(read.access_token(), access);
    assert_eq!(read.refresh_token(), refresh);
    assert_eq!(read.account_id(), Some("acct-123"));
    assert_eq!(read.refreshed_at_ms(), 42);
    let debug = format!("{read:?}");
    assert!(
        !debug.contains(&access) && !debug.contains(&refresh),
        "{debug}"
    );
    assert!(!debug.contains("acct-123"), "{debug}");

    let encoded = read.encode().unwrap();
    let line = std::str::from_utf8(&encoded).unwrap();
    assert!(
        line.starts_with(crate::egress_broker::rules::MANAGED_VALUE_PREFIX),
        "{line}"
    );
    assert!(
        line.starts_with(&format!("{{\"format\":\"{FORMAT}\"")),
        "{line}"
    );
    assert!(!line.contains('\n'));
    // What is kept: the access and refresh token and the account, nothing
    // else of auth.json (its id token and API key are not stored).
    let value: serde_json::Value = serde_json::from_str(line).unwrap();
    let mut keys: Vec<&str> = value
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "access_token",
            "account_id",
            "format",
            "refresh_token",
            "refreshed_at_ms"
        ]
    );
    assert!(!line.contains("sk-proj-"));
    // The secret store takes it as it is.
    let root = data_root();
    store(root.path(), &read);
    let again = stored(root.path());
    assert_eq!(again.access_token(), access);
    assert_eq!(again.refresh_token(), refresh);
    assert_eq!(again.account_id(), Some("acct-123"));

    // Without `tokens.account_id`, the id token's claim names the account.
    let mut value: serde_json::Value = serde_json::from_str(&text).unwrap();
    value["tokens"]["account_id"] = serde_json::Value::Null;
    let read = ChatgptSignIn::from_codex_auth_json(value.to_string().as_bytes(), 1).unwrap();
    assert_eq!(read.account_id(), Some("acct-123"));
    // Without `auth_mode` (older files), it is a ChatGPT sign-in too.
    value.as_object_mut().unwrap().remove("auth_mode");
    assert!(ChatgptSignIn::from_codex_auth_json(value.to_string().as_bytes(), 1).is_ok());
}

#[test]
fn what_is_not_a_chatgpt_sign_in_is_refused_without_showing_a_value() {
    let (access, refresh) = (access_token("acct-1", 3600), refresh_token());
    let key = format!("sk-proj-{}", uuid::Uuid::new_v4().simple());
    let cases = [
        serde_json::json!({"auth_mode": "apikey", "OPENAI_API_KEY": key}).to_string(),
        serde_json::json!({"auth_mode": "chatgpt", "OPENAI_API_KEY": key, "tokens": null})
            .to_string(),
        serde_json::json!({"tokens": {"access_token": access}}).to_string(),
        serde_json::json!({"tokens": {"access_token": access, "refresh_token": "two words"}})
            .to_string(),
        serde_json::json!({"tokens": {"access_token": format!("{access}\n"), "refresh_token": refresh}})
            .to_string(),
        format!("{{\"tokens\": {{\"access_token\": \"{access}\""),
        serde_json::json!({"auth_mode": format!("x{access}"), "tokens": {}}).to_string(),
    ];
    for case in cases {
        let error = ChatgptSignIn::from_codex_auth_json(case.as_bytes(), 1).unwrap_err();
        for value in [&access, &refresh, &key] {
            assert!(!error.contains(value.as_str()), "{error}");
        }
    }
    assert!(ChatgptSignIn::decode(access.as_bytes()).is_err());
    assert!(ChatgptSignIn::decode(b"{\"format\":\"axocoatl.other/1\"}").is_err());
}

#[test]
fn a_refresh_is_due_five_minutes_before_expiry_or_eight_days_after_a_sign_in() {
    let now = now_ms();
    let soon = sign_in(&access_token("a", 240), &refresh_token(), "a");
    assert!(soon.refresh_due(now));
    assert!(!soon.expired(now));
    let later = sign_in(&access_token("a", 3600), &refresh_token(), "a");
    assert!(!later.refresh_due(now));
    let gone = sign_in(&access_token("a", -10), &refresh_token(), "a");
    assert!(gone.refresh_due(now) && gone.expired(now));
    // Not a JWT: the eight-day rule.
    let opaque = |at| {
        ChatgptSignIn::new(
            Zeroizing::new("opaque-access".into()),
            Zeroizing::new(refresh_token()),
            None,
            at,
        )
        .unwrap()
    };
    assert!(!opaque(now - REFRESH_INTERVAL_MS + 60_000).refresh_due(now));
    assert!(opaque(now - REFRESH_INTERVAL_MS - 60_000).refresh_due(now));
    assert!(!opaque(0).expired(now));
}

/// The token endpoint's refresh answer.
fn refreshed(access: &str, refresh: Option<&str>) -> ResponseTemplate {
    let mut body = serde_json::json!({"access_token": access, "token_type": "Bearer",
        "expires_in": 3600, "id_token": jwt(serde_json::json!({
            "https://api.openai.com/auth": {"chatgpt_account_id": "acct-1"}}))});
    if let Some(refresh) = refresh {
        body["refresh_token"] = refresh.into();
    }
    ResponseTemplate::new(200).set_body_json(body)
}

fn refresh_request(refresh: &str) -> serde_json::Value {
    serde_json::json!({"client_id": CLIENT_ID, "grant_type": "refresh_token",
        "refresh_token": refresh})
}

fn broker(root: &Path, server: &MockServer) -> ChatgptBroker {
    ChatgptBroker::new(
        root,
        TokenEndpoint::for_test(&format!("{}/oauth/token", server.uri())),
    )
}

fn header(value: &ManagedValue, name: &str) -> Option<String> {
    value
        .headers
        .iter()
        .find(|(header, _)| header.as_str() == name)
        .map(|(_, value)| value.to_str().unwrap().to_string())
}

/// Before expiry the broker refreshes once, at the token endpoint with
/// Codex's client and grant, sends the new token with the account id,
/// stores the rotated sign-in atomically (`0600`, no temporary file left),
/// and does not refresh again while the new token is fresh.
#[tokio::test]
async fn an_expiring_token_is_refreshed_rotated_and_stored_atomically() {
    let root = data_root();
    let (old_access, old_refresh) = (access_token("acct-1", 60), refresh_token());
    store(root.path(), &sign_in(&old_access, &old_refresh, "acct-1"));
    let (new_access, new_refresh) = (access_token("acct-1", 3600), refresh_token());
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .and(body_json(refresh_request(&old_refresh)))
        .respond_with(refreshed(&new_access, Some(&new_refresh)))
        .expect(1)
        .mount(&server)
        .await;
    let broker = broker(root.path(), &server);
    let value = broker.current().await.unwrap();
    assert_eq!(value.secret.expose_secret(), new_access);
    assert_eq!(header(&value, ACCOUNT_HEADER).as_deref(), Some("acct-1"));
    let now = stored(root.path());
    assert_eq!(now.access_token(), new_access);
    assert_eq!(now.refresh_token(), new_refresh);
    let file = crate::secret_store::secret_path(root.path(), CODEX_CHATGPT_SECRET);
    assert_eq!(
        std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let entries: Vec<String> = std::fs::read_dir(file.parent().unwrap())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(entries, [CODEX_CHATGPT_SECRET]);
    // Fresh now: sent as stored, without another refresh.
    let value = broker.current().await.unwrap();
    assert_eq!(value.secret.expose_secret(), new_access);
    server.verify().await;
    // An answer without a refresh token keeps the one stored.
    let root = data_root();
    store(root.path(), &sign_in(&old_access, &old_refresh, "acct-1"));
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(refreshed(&new_access, None))
        .expect(1)
        .mount(&server)
        .await;
    broker_current(root.path(), &server).await.unwrap();
    assert_eq!(stored(root.path()).refresh_token(), old_refresh);
    assert_eq!(stored(root.path()).access_token(), new_access);
}

async fn broker_current(root: &Path, server: &MockServer) -> Result<ManagedValue, String> {
    broker(root, server).current().await
}

/// A `401` for the current token refreshes it once; a `401` for a token
/// that was already replaced does nothing.
#[tokio::test]
async fn a_rejected_token_is_refreshed_once() {
    let root = data_root();
    let (access, refresh) = (access_token("acct-1", 3600), refresh_token());
    store(root.path(), &sign_in(&access, &refresh, "acct-1"));
    let (new_access, new_refresh) = (access_token("acct-1", 3600), refresh_token());
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .and(body_json(refresh_request(&refresh)))
        .respond_with(refreshed(&new_access, Some(&new_refresh)))
        .expect(1)
        .mount(&server)
        .await;
    let broker = broker(root.path(), &server);
    assert_eq!(
        broker.current().await.unwrap().secret.expose_secret(),
        access
    );
    broker.rejected(&SecretString::from(access.clone())).await;
    assert_eq!(stored(root.path()).access_token(), new_access);
    // The same rejection reported again (a second request in flight).
    broker.rejected(&SecretString::from(access.clone())).await;
    assert_eq!(
        broker.current().await.unwrap().secret.expose_secret(),
        new_access
    );
    server.verify().await;
}

/// OpenAI's final refusal (here: a reused refresh token) is remembered for
/// that refresh token, nothing is stored, and an expired token is not
/// sent; a token that is still valid is sent when the refresh fails.
#[tokio::test]
async fn a_refused_refresh_is_not_retried_and_nothing_is_stored() {
    let root = data_root();
    let (access, refresh) = (access_token("acct-1", -5), refresh_token());
    store(root.path(), &sign_in(&access, &refresh, "acct-1"));
    let before = std::fs::read(crate::secret_store::secret_path(
        root.path(),
        CODEX_CHATGPT_SECRET,
    ))
    .unwrap();
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
            "error": {"code": "refresh_token_reused", "message": "already used"}})))
        .expect(1)
        .mount(&server)
        .await;
    let broker = broker(root.path(), &server);
    for _ in 0..2 {
        let error = broker.current().await.err().unwrap();
        assert!(error.contains("refresh_token_reused"), "{error}");
        assert!(error.contains("axocoatl connect codex"), "{error}");
        assert!(
            !error.contains(&refresh) && !error.contains(&access),
            "{error}"
        );
    }
    server.verify().await;
    let after = std::fs::read(crate::secret_store::secret_path(
        root.path(),
        CODEX_CHATGPT_SECRET,
    ))
    .unwrap();
    assert_eq!(before, after);

    // Unreachable endpoint, token still valid for four minutes: sent.
    let root = data_root();
    let valid = access_token("acct-1", 240);
    store(root.path(), &sign_in(&valid, &refresh_token(), "acct-1"));
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    let value = broker_current(root.path(), &server).await.unwrap();
    assert_eq!(value.secret.expose_secret(), valid);
    // A missing or mangled secret is refused with how to connect again.
    let root = data_root();
    let error = broker_current(root.path(), &server).await.err().unwrap();
    assert!(error.contains("cannot be read"), "{error}");
    crate::secret_store::set_secret(root.path(), CODEX_CHATGPT_SECRET, b"not-a-sign-in").unwrap();
    let error = broker_current(root.path(), &server).await.err().unwrap();
    assert!(error.contains("axocoatl connect codex"), "{error}");
}

/// A sign-in stored while a refresh was under way (a new
/// `axocoatl connect codex`) is kept, not overwritten by the refresh.
#[tokio::test]
async fn a_sign_in_stored_during_a_refresh_is_kept() {
    let root = data_root();
    let (access, refresh) = (access_token("acct-1", 60), refresh_token());
    store(root.path(), &sign_in(&access, &refresh, "acct-1"));
    let reconnected = (access_token("acct-2", 3600), refresh_token());
    let server = MockServer::start().await;
    {
        let root = root.path().to_path_buf();
        let reconnected = reconnected.clone();
        let refreshed_access = access_token("acct-1", 3600);
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .respond_with(move |_: &wiremock::Request| {
                store(&root, &sign_in(&reconnected.0, &reconnected.1, "acct-2"));
                refreshed(&refreshed_access, Some("rt_rotated"))
            })
            .expect(1)
            .mount(&server)
            .await;
    }
    let value = broker_current(root.path(), &server).await.unwrap();
    assert_eq!(value.secret.expose_secret(), reconnected.0);
    assert_eq!(header(&value, ACCOUNT_HEADER).as_deref(), Some("acct-2"));
    assert_eq!(stored(root.path()).refresh_token(), reconnected.1);
}

/// A plain read of the stored sign-in, as a route without its broker would
/// do, is refused: it holds a refresh token, which is never sent upstream.
#[test]
fn a_stored_sign_in_is_never_read_as_a_plain_credential() {
    let root = data_root();
    let (access, refresh) = (access_token("acct-1", 3600), refresh_token());
    store(root.path(), &sign_in(&access, &refresh, "acct-1"));
    let source = crate::egress_broker::CredentialSource::File(crate::secret_store::secret_path(
        root.path(),
        CODEX_CHATGPT_SECRET,
    ));
    let error = source
        .read(CODEX_CHATGPT_SECRET, &[])
        .unwrap_err()
        .to_string();
    assert!(error.contains("only on a route to its own host"), "{error}");
    assert!(
        !error.contains(&refresh) && !error.contains(&access),
        "{error}"
    );
    // The broker manages exactly that file, for chatgpt.com only: a route
    // to any other host naming it reads the file, which is refused above.
    let managed = managed_credentials(root.path());
    assert!(managed.find(&source, "chatgpt.com").is_some());
    for host in [
        "evil.test",
        "api.openai.com",
        "auth.openai.com",
        "chatgpt.com.evil.test",
    ] {
        assert!(managed.find(&source, host).is_none(), "{host}");
    }
    let other = crate::egress_broker::CredentialSource::File(crate::secret_store::secret_path(
        root.path(),
        "codex-openai",
    ));
    assert!(managed.find(&other, "chatgpt.com").is_none());
    assert!(managed
        .find(
            &crate::egress_broker::CredentialSource::Env("X".into()),
            "chatgpt.com"
        )
        .is_none());
}

/// A sign-in removed while a refresh was under way (`axocoatl secret remove
/// codex-chatgpt`) stays removed: the refreshed tokens go with that one
/// request and are not stored, and the next request is refused.
#[tokio::test]
async fn a_sign_in_removed_during_a_refresh_stays_removed() {
    let root = data_root();
    let (access, refresh) = (access_token("acct-1", 60), refresh_token());
    store(root.path(), &sign_in(&access, &refresh, "acct-1"));
    let refreshed_access = access_token("acct-1", 3600);
    let server = MockServer::start().await;
    {
        let root = root.path().to_path_buf();
        let refreshed_access = refreshed_access.clone();
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .respond_with(move |_: &wiremock::Request| {
                assert!(crate::secret_store::remove_secret(&root, CODEX_CHATGPT_SECRET).unwrap());
                refreshed(&refreshed_access, Some("rt_rotated"))
            })
            .expect(1)
            .mount(&server)
            .await;
    }
    let broker = broker(root.path(), &server);
    let value = broker.current().await.unwrap();
    assert_eq!(value.secret.expose_secret(), refreshed_access);
    let file = crate::secret_store::secret_path(root.path(), CODEX_CHATGPT_SECRET);
    assert!(!file.exists());
    let error = broker.current().await.err().unwrap();
    assert!(error.contains("cannot be read"), "{error}");
    assert!(!file.exists());
    server.verify().await;

    // Replaced meanwhile by a value that is not a sign-in: kept as it is.
    let root = data_root();
    store(root.path(), &sign_in(&access, &refresh, "acct-1"));
    let server = MockServer::start().await;
    {
        let root = root.path().to_path_buf();
        let refreshed_access = refreshed_access.clone();
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .respond_with(move |_: &wiremock::Request| {
                crate::secret_store::set_secret(&root, CODEX_CHATGPT_SECRET, b"something-else")
                    .unwrap();
                refreshed(&refreshed_access, Some("rt_rotated"))
            })
            .expect(1)
            .mount(&server)
            .await;
    }
    broker_current(root.path(), &server).await.unwrap();
    assert_eq!(
        std::fs::read(crate::secret_store::secret_path(
            root.path(),
            CODEX_CHATGPT_SECRET
        ))
        .unwrap(),
        b"something-else"
    );
}

/// A refreshed sign-in that cannot be stored is kept in memory and used
/// while the file still holds the sign-in it replaced, so the refresh token
/// OpenAI already rotated is not sent again; it is stored on a later
/// request.
#[tokio::test]
async fn a_refreshed_sign_in_that_cannot_be_stored_is_kept_and_stored_later() {
    let root = data_root();
    let (access, refresh) = (access_token("acct-1", 60), refresh_token());
    store(root.path(), &sign_in(&access, &refresh, "acct-1"));
    let (new_access, new_refresh) = (access_token("acct-1", 3600), refresh_token());
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .and(body_json(refresh_request(&refresh)))
        .respond_with(refreshed(&new_access, Some(&new_refresh)))
        .expect(1)
        .mount(&server)
        .await;
    let broker = broker(root.path(), &server);
    broker
        .fail_stores
        .store(2, std::sync::atomic::Ordering::SeqCst);
    // 1: refreshed; the write fails.
    let value = broker.current().await.unwrap();
    assert_eq!(value.secret.expose_secret(), new_access);
    assert_eq!(stored(root.path()).refresh_token(), refresh);
    // 2: the kept sign-in, no second refresh; the write fails again.
    let value = broker.current().await.unwrap();
    assert_eq!(value.secret.expose_secret(), new_access);
    assert_eq!(stored(root.path()).refresh_token(), refresh);
    // 3: stored now.
    let value = broker.current().await.unwrap();
    assert_eq!(value.secret.expose_secret(), new_access);
    assert_eq!(stored(root.path()).refresh_token(), new_refresh);
    assert_eq!(stored(root.path()).access_token(), new_access);
    server.verify().await;

    // Kept, but a new sign-in is stored meanwhile: the new one is used.
    let root = data_root();
    store(root.path(), &sign_in(&access, &refresh, "acct-1"));
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(refreshed(&new_access, Some(&new_refresh)))
        .expect(1)
        .mount(&server)
        .await;
    let broker = broker_for(root.path(), &server);
    broker
        .fail_stores
        .store(1, std::sync::atomic::Ordering::SeqCst);
    broker.current().await.unwrap();
    let reconnected = (access_token("acct-2", 3600), refresh_token());
    store(
        root.path(),
        &sign_in(&reconnected.0, &reconnected.1, "acct-2"),
    );
    let value = broker.current().await.unwrap();
    assert_eq!(value.secret.expose_secret(), reconnected.0);
    assert_eq!(stored(root.path()).refresh_token(), reconnected.1);
    // Kept, then removed: refused, and not stored again.
    let root = data_root();
    store(root.path(), &sign_in(&access, &refresh, "acct-1"));
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(refreshed(&new_access, Some(&new_refresh)))
        .expect(1)
        .mount(&server)
        .await;
    let broker = broker_for(root.path(), &server);
    broker
        .fail_stores
        .store(1, std::sync::atomic::Ordering::SeqCst);
    broker.current().await.unwrap();
    crate::secret_store::remove_secret(root.path(), CODEX_CHATGPT_SECRET).unwrap();
    assert!(broker.current().await.is_err());
    assert!(!crate::secret_store::secret_path(root.path(), CODEX_CHATGPT_SECRET).exists());
}

/// [`broker`], for a test whose local `broker` hides that function.
fn broker_for(root: &Path, server: &MockServer) -> ChatgptBroker {
    broker(root, server)
}

/// The broker reads the sign-in again and writes the refreshed one under
/// the secret store's lock, which `axocoatl connect codex` (`set_secret`)
/// and `axocoatl secret remove` take too: a sign-in stored while the lock was
/// held elsewhere is read and kept, never overwritten.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_broker_writes_under_the_secret_stores_lock() {
    let root = data_root();
    let (access, refresh) = (access_token("acct-1", 60), refresh_token());
    store(root.path(), &sign_in(&access, &refresh, "acct-1"));
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(refreshed(&access_token("acct-1", 3600), Some("rt_rotated")))
        .expect(1)
        .mount(&server)
        .await;
    let lock = crate::secret_store::lock_secrets(root.path(), false)
        .unwrap()
        .unwrap();
    let broker = Arc::new(broker(root.path(), &server));
    let refreshing = tokio::spawn({
        let broker = broker.clone();
        async move { broker.current().await.map(|value| value.secret) }
    });
    // The refresh is answered, and the broker waits for the lock.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while server
        .received_requests()
        .await
        .unwrap_or_default()
        .is_empty()
    {
        assert!(std::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!refreshing.is_finished());
    // What `connect codex` stores while holding the lock.
    let reconnected = (access_token("acct-2", 3600), refresh_token());
    lock.set(
        CODEX_CHATGPT_SECRET,
        &sign_in(&reconnected.0, &reconnected.1, "acct-2")
            .encode()
            .unwrap(),
    )
    .unwrap();
    drop(lock);
    let sent = refreshing.await.unwrap().unwrap();
    assert_eq!(sent.expose_secret(), reconnected.0);
    assert_eq!(stored(root.path()).refresh_token(), reconnected.1);
    server.verify().await;
}

/// `set_secret` and `remove_secret` wait for the store's lock.
#[test]
fn storing_and_removing_a_secret_wait_for_the_lock() {
    let root = data_root();
    store(
        root.path(),
        &sign_in(&access_token("acct-1", 3600), &refresh_token(), "acct-1"),
    );
    let lock = crate::secret_store::lock_secrets(root.path(), false)
        .unwrap()
        .unwrap();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let path = root.path().to_path_buf();
    let remover = std::thread::spawn(move || {
        let removed = crate::secret_store::remove_secret(&path, CODEX_CHATGPT_SECRET).unwrap();
        done_tx.send(removed).unwrap();
    });
    assert!(done_rx.recv_timeout(Duration::from_millis(300)).is_err());
    assert!(crate::secret_store::secret_path(root.path(), CODEX_CHATGPT_SECRET).exists());
    drop(lock);
    assert!(done_rx.recv_timeout(Duration::from_secs(5)).unwrap());
    remover.join().unwrap();
    assert!(!crate::secret_store::secret_path(root.path(), CODEX_CHATGPT_SECRET).exists());
}

#[path = "codex_chatgpt_route_tests.rs"]
mod route;
