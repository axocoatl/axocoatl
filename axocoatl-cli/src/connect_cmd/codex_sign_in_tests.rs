//! `connect codex --refresh` against a fake daemon (what each answer comes
//! to: message and exit code), `connect codex --status` on stored sign-ins,
//! and `--sign-in-timeout`'s bounds, lifetime and messages. Nothing here
//! reaches a network or a real account.

use super::*;
use crate::connect_cmd::{codex_container, ConnectCommands};
use axocoatl_daemon::external_agent::codex_chatgpt::ChatgptSignIn;
use base64::Engine as _;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};
use zeroize::Zeroizing;

const HOUR_MS: u64 = 3600 * 1000;

/// An unsigned JWT expiring at `exp_ms` (a fresh random part makes each one
/// different).
fn jwt(exp_ms: u64) -> String {
    let encode = |value: serde_json::Value| {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(value.to_string())
    };
    format!(
        "{}.{}.{}",
        encode(serde_json::json!({"alg": "RS256"})),
        encode(serde_json::json!({"exp": exp_ms / 1000, "jti": uuid::Uuid::new_v4()})),
        uuid::Uuid::new_v4().simple()
    )
}

fn sign_in(access: &str, account: Option<&str>, refreshed_at_ms: u64) -> ChatgptSignIn {
    ChatgptSignIn::new(
        Zeroizing::new(access.to_string()),
        Zeroizing::new(format!("rt_{}", uuid::Uuid::new_v4().simple())),
        account.map(str::to_string),
        refreshed_at_ms,
    )
    .unwrap()
}

/// The fake daemon answers `status` with `body` to the refresh request,
/// once, and only with the API token.
async fn daemon_answering(status: u16, body: serde_json::Value) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(REFRESH_PATH))
        .and(header("authorization", "Bearer local-token"))
        .respond_with(ResponseTemplate::new(status).set_body_json(body))
        .expect(1)
        .mount(&server)
        .await;
    server
}

async fn refresh_at(server: &MockServer) -> Result<String, Failure> {
    refresh(&crate::run_cmd::Daemon::for_test(
        &server.uri(),
        Some("local-token"),
    ))
    .await
}

/// Each of the daemon's answers comes to its message and exit code: a
/// refresh prints the new token's expiry and the time of the refresh;
/// a refusal, no sign-in and a failed refresh have exit codes of their own.
#[tokio::test]
async fn each_daemon_answer_has_its_message_and_exit_code() {
    let now = now_ms();
    let (refreshed_at, expires_at) = (now - 1000, now + HOUR_MS);
    let server = daemon_answering(
        200,
        serde_json::json!({"outcome": "refreshed", "refreshed_at_ms": refreshed_at,
            "expires_at_ms": expires_at}),
    )
    .await;
    let message = refresh_at(&server).await.unwrap();
    assert!(
        message.starts_with("Refreshed the ChatGPT sign-in at "),
        "{message}"
    );
    assert!(message.contains(&local_time(refreshed_at)), "{message}");
    assert!(
        message.contains(&format!(
            "The new access token expires at {} (in ",
            local_time(expires_at)
        )),
        "{message}"
    );
    assert!(message.contains("no token was shown"), "{message}");
    server.verify().await;

    let server = daemon_answering(
        200,
        serde_json::json!({"outcome": "replaced", "refreshed_at_ms": refreshed_at,
            "expires_at_ms": null}),
    )
    .await;
    let message = refresh_at(&server).await.unwrap();
    assert!(message.contains("is kept instead"), "{message}");

    for (status, body, code, words) in [
        (
            409,
            serde_json::json!({"outcome": "refused", "sent": true,
                "message": "OpenAI refused to refresh the ChatGPT sign-in (HTTP 400, invalid_grant); connect again with `axocoatl connect codex`"}),
            exit::REJECTED,
            "invalid_grant",
        ),
        (
            404,
            serde_json::json!({"outcome": "no_sign_in",
                "message": "no ChatGPT sign-in is stored as codex-chatgpt; sign in with `axocoatl connect codex`"}),
            exit::NO_TOKEN,
            "no ChatGPT sign-in is stored",
        ),
        (
            502,
            serde_json::json!({"outcome": "failed",
                "message": "the ChatGPT sign-in could not be refreshed: the token endpoint could not be reached; the stored sign-in is unchanged"}),
            exit::UNVERIFIED,
            "could not be reached",
        ),
        (
            422,
            serde_json::json!({"outcome": "unusable", "message": "cannot be used"}),
            exit::FAILURE,
            "cannot be used",
        ),
        (
            500,
            serde_json::json!({"outcome": "not_stored", "refreshed_at_ms": refreshed_at,
                "expires_at_ms": expires_at, "message": "the refreshed sign-in could not be written"}),
            exit::FAILURE,
            "could not be written",
        ),
        (
            503,
            serde_json::json!({"outcome": "unavailable", "message": "no broker"}),
            exit::FAILURE,
            "no broker",
        ),
        // Not a report: the API token refused, the route refused, or a
        // daemon without the route.
        (
            401,
            serde_json::json!({}),
            exit::FAILURE,
            "refused the local API token",
        ),
        (
            403,
            serde_json::json!({"error": "only from this computer"}),
            exit::FAILURE,
            "only from this computer",
        ),
        (
            404,
            serde_json::json!({}),
            exit::FAILURE,
            "older than Axocoatl 1.3.1",
        ),
    ] {
        let server = daemon_answering(status, body.clone()).await;
        let failed = refresh_at(&server).await.unwrap_err();
        assert_eq!(failed.code, code, "{body}: {}", failed.message);
        assert!(failed.message.contains(words), "{body}: {}", failed.message);
        server.verify().await;
    }
}

/// No daemon at the address: its own exit code, and how to start one.
#[tokio::test]
async fn no_daemon_running_has_its_own_exit_code() {
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let failed = refresh(&crate::run_cmd::Daemon::for_test(
        &format!("http://127.0.0.1:{port}"),
        Some("local-token"),
    ))
    .await
    .unwrap_err();
    assert_eq!(failed.code, exit::DAEMON_NOT_RUNNING);
    assert_eq!(exit::DAEMON_NOT_RUNNING, 7);
    assert!(
        failed.message.contains("axocoatl serve"),
        "{}",
        failed.message
    );
    assert!(
        failed.message.contains(&format!("127.0.0.1:{port}")),
        "{}",
        failed.message
    );
}

/// `--status`: stored or not, the account id by its last four characters,
/// the access token's expiry (or that it expired) and the time of the last
/// refresh or sign-in; never a token, never the whole account id.
#[test]
fn status_shows_the_expiry_and_the_masked_account_without_a_token() {
    let data = Path::new("/data/root");
    let now = now_ms();
    let access = jwt(now + HOUR_MS);
    let stored = sign_in(&access, Some("acct-0123-4567-wxyz"), now - 2 * HOUR_MS);
    let refresh_token = stored.refresh_token().to_string();
    let text = status_text(Ok(Some(stored)), data, now).unwrap();
    assert!(
        text.starts_with("A ChatGPT sign-in is stored (secret codex-chatgpt in /data/root)"),
        "{text}"
    );
    assert!(text.contains("Account: …wxyz\n"), "{text}");
    assert!(!text.contains("acct-0123"), "{text}");
    let expires = (now + HOUR_MS) / 1000 * 1000;
    assert!(
        text.contains(&format!(
            "Access token: expires at {} (in 59 minutes)",
            local_time(expires)
        )) || text.contains(&format!(
            "Access token: expires at {} (in 1 hour)",
            local_time(expires)
        )),
        "{text}"
    );
    assert!(
        text.contains(&format!(
            "Last refreshed or signed in: {} (2 hours ago)",
            local_time(now - 2 * HOUR_MS)
        )),
        "{text}"
    );
    assert!(
        !text.contains(&access) && !text.contains(&refresh_token),
        "{text}"
    );

    let expired = sign_in(&jwt(now - 60_000), None, now - 9 * 24 * HOUR_MS);
    let text = status_text(Ok(Some(expired)), data, now).unwrap();
    assert!(text.contains("Account: none stored"), "{text}");
    assert!(text.contains("Access token: expired at "), "{text}");
    assert!(text.contains("(1 minute ago)"), "{text}");
    assert!(text.contains("(9 days ago)"), "{text}");

    let opaque = sign_in("opaque-access-token", Some("abcd"), now);
    let text = status_text(Ok(Some(opaque)), data, now).unwrap();
    assert!(text.contains("carries no expiry time"), "{text}");
    assert!(text.contains("Account: (hidden"), "{text}");
    assert!(!text.contains("abcd"), "{text}");

    let none = status_text(Ok(None), data, now).unwrap_err();
    assert_eq!(none.code, exit::NO_TOKEN);
    assert!(
        none.message.contains("axocoatl connect codex"),
        "{}",
        none.message
    );
    let unusable = status_text(Err("it cannot be used".into()), data, now).unwrap_err();
    assert_eq!(unusable.code, exit::FAILURE);
}

/// `--status` reads the data root: nothing stored, then a stored sign-in.
#[test]
fn status_reads_the_sign_in_stored_in_the_data_root() {
    let root = tempfile::tempdir().unwrap();
    std::fs::set_permissions(
        root.path(),
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .unwrap();
    assert!(read_stored(root.path()).unwrap().is_none());
    let now = now_ms();
    let stored = sign_in(&jwt(now + HOUR_MS), Some("acct-status-9876"), now);
    axocoatl_daemon::secret_store::set_secret(
        root.path(),
        CODEX_CHATGPT_SECRET,
        &stored.encode().unwrap(),
    )
    .unwrap();
    let text = status_text(read_stored(root.path()), root.path(), now).unwrap();
    assert!(text.contains("Account: …9876"), "{text}");
    assert!(text.contains("Access token: expires at "), "{text}");
    axocoatl_daemon::secret_store::set_secret(root.path(), CODEX_CHATGPT_SECRET, b"other").unwrap();
    let failed = status_text(read_stored(root.path()), root.path(), now).unwrap_err();
    assert_eq!(failed.code, exit::FAILURE);
    assert!(!failed.message.contains("other"), "{}", failed.message);
}

#[test]
fn times_are_in_words() {
    let now = 1_000_000_000_000;
    assert_eq!(relative(now, now), "in 0 seconds");
    assert_eq!(relative(now + 1000, now), "in 1 second");
    assert_eq!(relative(now + 59 * 60_000, now), "in 59 minutes");
    assert_eq!(relative(now - 3 * HOUR_MS, now), "3 hours ago");
    assert_eq!(relative(now + 47 * HOUR_MS, now), "in 47 hours");
    assert_eq!(relative(now - 72 * HOUR_MS, now), "3 days ago");
    assert_eq!(masked_account_id("acct-12345"), "…2345");
    assert_eq!(masked_account_id("12345"), "…2345");
    assert!(!masked_account_id("1234").contains("1234"));
}

fn parse(argv: &[&str]) -> Result<ConnectCommands, clap::Error> {
    use clap::Parser;
    #[derive(Parser)]
    struct Cli {
        #[command(subcommand)]
        command: ConnectCommands,
    }
    let mut full = vec!["x"];
    full.extend_from_slice(argv);
    Cli::try_parse_from(full).map(|cli| cli.command)
}

/// `--sign-in-timeout` takes 1 to 60 minutes (30 without it) and only for
/// the login container; `--refresh` and `--status` take none of the
/// sign-in's options, and `--url` only goes with `--refresh`.
#[test]
fn the_options_are_bounded_and_exclusive() {
    for minutes in ["0", "61", "-1", "half", "1.5"] {
        assert!(
            parse(&["codex", "--sign-in-timeout", minutes]).is_err(),
            "{minutes}"
        );
    }
    for (minutes, seconds) in [("1", 60), ("15", 900), ("60", 3600)] {
        let Ok(ConnectCommands::Codex {
            sign_in_timeout, ..
        }) = parse(&["codex", "--sign-in-timeout", minutes])
        else {
            panic!("{minutes}")
        };
        assert_eq!(
            codex_container::sign_in_lifetime(sign_in_timeout).as_secs(),
            seconds
        );
    }
    let Ok(ConnectCommands::Codex {
        sign_in_timeout,
        refresh,
        status,
        url,
        ..
    }) = parse(&["codex"])
    else {
        panic!("codex")
    };
    assert_eq!(sign_in_timeout, None);
    assert!(!refresh && !status && url.is_none());
    assert_eq!(
        codex_container::sign_in_lifetime(None),
        codex_container::DEFAULT_LIFETIME
    );
    assert_eq!(codex_container::DEFAULT_LIFETIME.as_secs(), 30 * 60);
    assert!(parse(&["codex", "--sign-in-timeout", "5", "--codex", "/opt/codex"]).is_err());
    assert!(parse(&["codex", "--sign-in-timeout", "5", "--device-auth"]).is_ok());
    for argv in [
        &["codex", "--refresh", "--device-auth"][..],
        &["codex", "--refresh", "--codex", "/opt/codex"],
        &["codex", "--refresh", "--no-browser"],
        &["codex", "--refresh", "--sign-in-timeout", "5"],
        &["codex", "--refresh", "--status"],
        &["codex", "--status", "--device-auth"],
        &["codex", "--status", "--sign-in-timeout", "5"],
        &["codex", "--url", "http://127.0.0.1:8080"],
        &["codex", "--status", "--url", "http://127.0.0.1:8080"],
    ] {
        assert!(parse(argv).is_err(), "{argv:?}");
    }
    let Ok(ConnectCommands::Codex { refresh, url, .. }) =
        parse(&["codex", "--refresh", "--url", "http://127.0.0.1:9"])
    else {
        panic!("refresh")
    };
    assert!(refresh);
    assert_eq!(url.as_deref(), Some("http://127.0.0.1:9"));
    let Ok(ConnectCommands::Codex { status, .. }) = parse(&["codex", "--status"]) else {
        panic!("status")
    };
    assert!(status);
}

/// The intro's line and the timeout message say the time chosen.
#[test]
fn the_sign_in_time_is_in_the_messages() {
    let five = codex_container::sign_in_lifetime(Some(5));
    assert_eq!(
        codex_container::time_to_sign_in(five),
        "You have 5 minutes to finish signing in."
    );
    assert_eq!(
        codex_container::time_to_sign_in(codex_container::sign_in_lifetime(Some(1))),
        "You have 1 minute to finish signing in."
    );
    assert_eq!(
        codex_container::timed_out(five),
        "The Codex sign-in was not completed within 5 minutes; nothing was stored. Run \
         `axocoatl connect codex` again."
    );
    let args = codex_container::run_args("n", "i", false, five);
    assert!(args.iter().any(|arg| arg == "--timeout=300"), "{args:?}");
    let args =
        codex_container::run_args("n", "i", true, codex_container::sign_in_lifetime(Some(60)));
    assert!(args.iter().any(|arg| arg == "--timeout=3600"), "{args:?}");
}
