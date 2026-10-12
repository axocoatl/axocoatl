//! [`ChatgptBroker::refresh_now`], what `axocoatl connect codex --refresh`
//! asks the daemon for, against a fake OpenAI token endpoint: it refreshes a
//! token that is not due, through the same request, rotation, atomic write
//! and remembered refusal as the automatic refresh, and says what happened
//! without a token. Nothing here reaches OpenAI. The daemon log's record of
//! it is checked end to end in `axocoatl-cli/tests/connect_codex_refresh.rs`,
//! where no other test shares the process's log.

use super::*;

/// The report as the API sends it, checked to hold none of `tokens`.
fn json_without(report: &RefreshReport, tokens: &[&str]) -> serde_json::Value {
    let text = serde_json::to_string(report).unwrap();
    for token in tokens {
        assert!(!text.contains(token), "{text}");
    }
    serde_json::from_str(&text).unwrap()
}

fn file_bytes(root: &Path) -> Vec<u8> {
    std::fs::read(crate::secret_store::secret_path(root, CODEX_CHATGPT_SECRET)).unwrap()
}

/// A token an hour from expiry is not due, and is refreshed anyway: the
/// stored refresh token goes to the token endpoint with Codex's client and
/// grant, the rotated sign-in is stored atomically (`0600`, nothing else
/// left in the secrets directory), and the report has the new token's
/// expiry and the time of the refresh. A second refresh sends the rotated
/// refresh token, never the used one.
#[tokio::test]
async fn a_refresh_on_request_rotates_and_stores_the_sign_in() {
    let root = data_root();
    let (access, refresh) = (access_token("acct-1", 3600), refresh_token());
    store(root.path(), &sign_in(&access, &refresh, "acct-1"));
    let (new_access, new_refresh) = (access_token("acct-1", 7200), refresh_token());
    let (third_access, third_refresh) = (access_token("acct-1", 7200), refresh_token());
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .and(body_json(refresh_request(&refresh)))
        .respond_with(refreshed(&new_access, Some(&new_refresh)))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .and(body_json(refresh_request(&new_refresh)))
        .respond_with(refreshed(&third_access, Some(&third_refresh)))
        .expect(1)
        .mount(&server)
        .await;
    let broker = broker(root.path(), &server);
    assert!(!stored(root.path()).refresh_due(now_ms()));
    let before = now_ms();
    let report = broker.refresh_now().await;
    let after = now_ms();
    let RefreshReport::Refreshed {
        refreshed_at_ms,
        expires_at_ms,
    } = report.clone()
    else {
        panic!("{report:?}");
    };
    assert!((before..=after).contains(&refreshed_at_ms));
    let now = stored(root.path());
    assert_eq!(now.access_token(), new_access);
    assert_eq!(now.refresh_token(), new_refresh);
    assert_eq!(now.account_id(), Some("acct-1"));
    assert_eq!(now.refreshed_at_ms(), refreshed_at_ms);
    assert_eq!(expires_at_ms, now.expires_at_ms());
    assert!(expires_at_ms.unwrap() > after + 7000 * 1000);
    let json = json_without(&report, &[&access, &refresh, &new_access, &new_refresh]);
    assert_eq!(json["outcome"], "refreshed");
    assert_eq!(json["expires_at_ms"], expires_at_ms.unwrap());
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
    // The route sends the new token as it is (not due).
    let value = broker.current().await.unwrap();
    assert_eq!(value.secret.expose_secret(), new_access);
    // Again: the rotated refresh token goes.
    let report = broker.refresh_now().await;
    assert_eq!(report.outcome(), "refreshed", "{report:?}");
    assert_eq!(stored(root.path()).refresh_token(), third_refresh);
    server.verify().await;
}

/// A refresh token OpenAI refuses for good is remembered by the broker the
/// route uses too: the next refresh on request, and the route's own, do not
/// send it again; nothing is stored. The report says so without a token.
#[tokio::test]
async fn a_refused_refresh_on_request_is_remembered_and_not_sent_again() {
    let root = data_root();
    let (access, refresh) = (access_token("acct-1", 60), refresh_token());
    store(root.path(), &sign_in(&access, &refresh, "acct-1"));
    let before = file_bytes(root.path());
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
            "error": "invalid_grant", "error_description": "refresh token is invalid"})))
        .expect(1)
        .mount(&server)
        .await;
    let broker = broker(root.path(), &server);
    let report = broker.refresh_now().await;
    let RefreshReport::Refused { message, sent } = &report else {
        panic!("{report:?}");
    };
    assert!(*sent);
    assert!(message.contains("HTTP 400, invalid_grant"), "{message}");
    assert!(message.contains("axocoatl connect codex"), "{message}");
    json_without(&report, &[&access, &refresh]);
    let report = broker.refresh_now().await;
    let RefreshReport::Refused { message, sent } = &report else {
        panic!("{report:?}");
    };
    assert!(!*sent);
    assert!(message.contains("was not sent again"), "{message}");
    // The route's refresh (the token is due) remembers it too; the token
    // is still valid for a minute, so it is sent.
    let value = broker.current().await.unwrap();
    assert_eq!(value.secret.expose_secret(), access);
    server.verify().await;
    assert_eq!(file_bytes(root.path()), before);
}

/// No sign-in stored, or one that cannot be used: nothing is sent.
#[tokio::test]
async fn a_refresh_on_request_without_a_usable_sign_in_sends_nothing() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;
    let root = data_root();
    let report = broker(root.path(), &server).refresh_now().await;
    let RefreshReport::NoSignIn { message } = &report else {
        panic!("{report:?}");
    };
    assert!(
        message.contains("no ChatGPT sign-in is stored"),
        "{message}"
    );
    assert!(message.contains("axocoatl connect codex"), "{message}");
    crate::secret_store::set_secret(root.path(), CODEX_CHATGPT_SECRET, b"not-a-sign-in").unwrap();
    let report = broker(root.path(), &server).refresh_now().await;
    let RefreshReport::Unusable { message } = &report else {
        panic!("{report:?}");
    };
    assert!(message.contains("cannot be used"), "{message}");
    assert!(!message.contains("not-a-sign-in"), "{message}");
    server.verify().await;
}

/// An endpoint that cannot be reached, or answers neither tokens nor a
/// refusal: the refresh fails and the stored sign-in is unchanged.
#[tokio::test]
async fn a_failed_refresh_on_request_changes_nothing() {
    let root = data_root();
    let (access, refresh) = (access_token("acct-1", 3600), refresh_token());
    store(root.path(), &sign_in(&access, &refresh, "acct-1"));
    let before = file_bytes(root.path());
    // Nothing listens on a port that was just free.
    let closed = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let unreachable = ChatgptBroker::new(
        root.path(),
        TokenEndpoint::for_test(&format!("http://127.0.0.1:{closed}/oauth/token")),
    );
    let report = unreachable.refresh_now().await;
    let RefreshReport::Failed { message } = &report else {
        panic!("{report:?}");
    };
    assert!(message.contains("could not be reached"), "{message}");
    assert!(message.contains("unchanged"), "{message}");
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(503))
        .expect(1)
        .mount(&server)
        .await;
    let report = broker(root.path(), &server).refresh_now().await;
    let RefreshReport::Failed { message } = &report else {
        panic!("{report:?}");
    };
    assert!(message.contains("HTTP 503"), "{message}");
    json_without(&report, &[&access, &refresh]);
    assert_eq!(file_bytes(root.path()), before);
}

/// What became of the new tokens: kept in memory when they cannot be
/// stored (and stored later), dropped for a sign-in stored meanwhile, and
/// not stored when the sign-in was removed meanwhile.
#[tokio::test]
async fn a_refresh_on_request_says_what_became_of_the_new_tokens() {
    // Not stored: kept, used and stored on the next use.
    let root = data_root();
    let (access, refresh) = (access_token("acct-1", 3600), refresh_token());
    store(root.path(), &sign_in(&access, &refresh, "acct-1"));
    let (new_access, new_refresh) = (access_token("acct-1", 7200), refresh_token());
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(refreshed(&new_access, Some(&new_refresh)))
        .expect(1)
        .mount(&server)
        .await;
    let kept = broker(root.path(), &server);
    kept.fail_stores
        .store(1, std::sync::atomic::Ordering::SeqCst);
    let report = kept.refresh_now().await;
    assert_eq!(report.outcome(), "not_stored", "{report:?}");
    assert_eq!(stored(root.path()).refresh_token(), refresh);
    let value = kept.current().await.unwrap();
    assert_eq!(value.secret.expose_secret(), new_access);
    assert_eq!(stored(root.path()).refresh_token(), new_refresh);
    server.verify().await;

    // A sign-in stored meanwhile is kept: its times are reported.
    let root = data_root();
    store(root.path(), &sign_in(&access, &refresh, "acct-1"));
    let reconnected = (access_token("acct-2", 5400), refresh_token());
    let server = MockServer::start().await;
    {
        let root = root.path().to_path_buf();
        let reconnected = reconnected.clone();
        let new_access = new_access.clone();
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .respond_with(move |_: &wiremock::Request| {
                store(&root, &sign_in(&reconnected.0, &reconnected.1, "acct-2"));
                refreshed(&new_access, Some("rt_rotated"))
            })
            .expect(1)
            .mount(&server)
            .await;
    }
    let report = broker(root.path(), &server).refresh_now().await;
    let RefreshReport::Replaced { expires_at_ms, .. } = report else {
        panic!("{report:?}");
    };
    assert_eq!(expires_at_ms, stored(root.path()).expires_at_ms());
    assert_eq!(stored(root.path()).refresh_token(), reconnected.1);

    // Removed meanwhile: not stored, and it says there is no sign-in.
    let root = data_root();
    store(root.path(), &sign_in(&access, &refresh, "acct-1"));
    let server = MockServer::start().await;
    {
        let root = root.path().to_path_buf();
        let new_access = new_access.clone();
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .respond_with(move |_: &wiremock::Request| {
                crate::secret_store::remove_secret(&root, CODEX_CHATGPT_SECRET).unwrap();
                refreshed(&new_access, Some("rt_rotated"))
            })
            .expect(1)
            .mount(&server)
            .await;
    }
    let report = broker(root.path(), &server).refresh_now().await;
    let RefreshReport::NoSignIn { message } = &report else {
        panic!("{report:?}");
    };
    assert!(message.contains("removed"), "{message}");
    assert!(!crate::secret_store::secret_path(root.path(), CODEX_CHATGPT_SECRET).exists());
}

/// The daemon's route to `chatgpt.com` and `axocoatl connect codex
/// --refresh` use one broker (`managed_credentials_of`, as the daemon's
/// Session decision points hold it): one lock, one remembered refusal.
#[test]
fn the_routes_managed_credential_is_the_broker_that_refreshes_on_request() {
    let root = data_root();
    let broker = Arc::new(ChatgptBroker::new(
        root.path(),
        TokenEndpoint::for_test("http://127.0.0.1:9/oauth/token"),
    ));
    let managed = managed_credentials_of(Some(broker.clone()));
    let source = crate::egress_broker::rules::CredentialSource::File(
        crate::secret_store::secret_path(root.path(), CODEX_CHATGPT_SECRET),
    );
    let found = managed
        .find(&source, crate::external_agent::codex::CHATGPT_HOST)
        .expect("the route finds the broker");
    assert!(std::ptr::eq(
        Arc::as_ptr(&found) as *const (),
        Arc::as_ptr(&broker) as *const ()
    ));
    assert!(managed_credentials_of(None)
        .find(&source, crate::external_agent::codex::CHATGPT_HOST)
        .is_none());
}
