//! The verification-only stale-access switch, read from the environment the
//! way the daemon reads it: `AXOCOATL_VERIFY_STALE_ACCESS_ONCE=1` is set in
//! this test's own process (this file holds one test, so nothing else in the
//! process sees it), then the daemon's managed credentials are made.
//!
//! Without the `live-verify` feature, the variable does nothing: the broker
//! sends the stored access token every time and logs nothing about it. With
//! it (`cargo test -p axocoatl-daemon --features live-verify --test
//! live_verify_switch`), the first value of the process is the switch's
//! invalid token and every later one, from any broker of the process, the
//! stored token, and the daemon's log says so at `WARN` without a token.
//!
//! The stored token here does not expire for an hour and nothing is answered
//! `401`, so no refresh is attempted and nothing leaves this computer; the
//! refresh and the retry are exercised against fake servers in the crate's
//! own tests (`external_agent::codex_chatgpt`).

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::{Arc, Mutex};

use axocoatl_daemon::egress_broker::rules::ManagedCredential;
use axocoatl_daemon::egress_broker::CredentialSource;
use axocoatl_daemon::external_agent::codex_chatgpt::{
    managed_credentials, ChatgptSignIn, CODEX_CHATGPT_SECRET,
};
use base64::Engine as _;
use secrecy::ExposeSecret;
use zeroize::Zeroizing;

const VARIABLE: &str = "AXOCOATL_VERIFY_STALE_ACCESS_ONCE";

/// An unsigned access token for `acct-1` that expires in an hour.
fn access_token() -> String {
    let encode = |value: serde_json::Value| {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(value.to_string())
    };
    let exp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 3600;
    format!(
        "{}.{}.{}",
        encode(serde_json::json!({"alg": "RS256", "typ": "JWT"})),
        encode(serde_json::json!({
            "exp": exp,
            "jti": uuid::Uuid::new_v4().to_string(),
            "https://api.openai.com/auth": {"chatgpt_account_id": "acct-1"},
        })),
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(uuid::Uuid::new_v4().as_bytes())
    )
}

/// The value the daemon's broker for the sign-in under `root` sends now,
/// from a broker made afresh.
async fn current(root: &Path) -> String {
    let source = CredentialSource::File(axocoatl_daemon::secret_store::secret_path(
        root,
        CODEX_CHATGPT_SECRET,
    ));
    let broker = managed_credentials(root)
        .find(&source, "chatgpt.com")
        .expect("the daemon manages the ChatGPT sign-in");
    current_of(broker.as_ref()).await
}

async fn current_of(broker: &dyn ManagedCredential) -> String {
    broker
        .current()
        .await
        .unwrap()
        .secret
        .expose_secret()
        .to_string()
}

/// The daemon's log, kept in memory.
#[derive(Clone, Default)]
struct Log(Arc<Mutex<Vec<u8>>>);

impl Write for Log {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Log {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

#[tokio::test(flavor = "current_thread")]
async fn the_variable_turns_the_switch_on_only_in_a_live_verify_build() {
    let log = Log::default();
    let writer = log.clone();
    let _subscriber = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish(),
    );
    // As the daemon starts: the variable is set before its managed
    // credentials are made.
    std::env::set_var(VARIABLE, "1");

    let root = tempfile::tempdir().unwrap();
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let access = access_token();
    let refresh = format!("rt_{}", uuid::Uuid::new_v4().simple());
    let sign_in = ChatgptSignIn::new(
        Zeroizing::new(access.clone()),
        Zeroizing::new(refresh.clone()),
        Some("acct-1".into()),
        0,
    )
    .unwrap();
    axocoatl_daemon::secret_store::set_secret(
        root.path(),
        CODEX_CHATGPT_SECRET,
        &sign_in.encode().unwrap(),
    )
    .unwrap();

    let source = CredentialSource::File(axocoatl_daemon::secret_store::secret_path(
        root.path(),
        CODEX_CHATGPT_SECRET,
    ));
    let broker = managed_credentials(root.path())
        .find(&source, "chatgpt.com")
        .unwrap();
    let first = current_of(broker.as_ref()).await;
    let second = current_of(broker.as_ref()).await;
    // A broker the same process makes later shares the spent switch.
    let later = current(root.path()).await;
    let text = log.text();

    #[cfg(not(feature = "live-verify"))]
    {
        assert_eq!([&first, &second, &later], [&access, &access, &access]);
        assert!(!text.contains("VERIFICATION SWITCH"), "{text}");
        assert!(!text.contains(VARIABLE), "{text}");
    }
    #[cfg(feature = "live-verify")]
    {
        use axocoatl_daemon::external_agent::codex_chatgpt::verify::{
            STALE_ACCESS_ENV, STALE_ACCESS_TOKEN,
        };
        assert_eq!(STALE_ACCESS_ENV, VARIABLE);
        assert_eq!(first, STALE_ACCESS_TOKEN);
        assert_eq!([&second, &later], [&access, &access]);
        let active = text
            .lines()
            .find(|line| line.contains("VERIFICATION SWITCH ACTIVE"))
            .unwrap_or_else(|| panic!("{text}"));
        assert!(active.contains("WARN"), "{active}");
        assert!(active.contains(VARIABLE), "{active}");
        let fired: Vec<&str> = text
            .lines()
            .filter(|line| line.contains("this request to chatgpt.com carries an invalid"))
            .collect();
        assert_eq!(fired.len(), 1, "{text}");
        assert!(fired[0].contains("WARN"), "{}", fired[0]);
        assert!(!text.contains(STALE_ACCESS_TOKEN), "{text}");
    }
    assert!(
        !text.contains(&access) && !text.contains(&refresh),
        "{text}"
    );
    std::env::remove_var(VARIABLE);
}
