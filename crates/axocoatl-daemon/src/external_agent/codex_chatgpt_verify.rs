//! Verification only: the stale-access switch of the ChatGPT sign-in's
//! broker. It is compiled only with the `live-verify` cargo feature of
//! `axocoatl-daemon` (which `axocoatl-cli`'s `live-verify` feature turns on),
//! and in this crate's tests. The feature is off by default. No release build
//! and no binary CI builds turns it on: `scripts/check-no-live-verify.sh`
//! refuses a release binary that holds [`STALE_ACCESS_ENV`] (run by
//! `scripts/verify-native-gate.sh`, `scripts/verify-cross-linux-gate.sh` and
//! the release workflow's build job), and `scripts/check-workflow-contracts.rb`
//! refuses a manifest, workflow or script that would turn it on. CI's
//! `cargo check` and `cargo clippy` with `--all-features` type-check and lint
//! it without building a binary.
//!
//! In a build with the feature, when [`STALE_ACCESS_ENV`] is `1` as the
//! daemon starts, the broker sends [`STALE_ACCESS_TOKEN`] (a fixed JWT with
//! no valid signature, expired in 1970) in place of the stored access token
//! with the first request of the process it gives a value for. When the
//! upstream answers that request `401`, the broker's path for a `401` runs
//! against the real service: one refresh at the token endpoint, the rotated
//! sign-in stored, and the next request carrying the refreshed token. The
//! `401` reaches the program as any other does. The switch fires once per
//! process, however many brokers the process makes, and the daemon logs at
//! `WARN` that it is active, when it fires, and when its `401` arrives.

use std::sync::atomic::{AtomicU8, Ordering};

/// The environment variable that turns the switch on (`1`) as the daemon
/// starts. Read only in a build with the `live-verify` feature.
pub const STALE_ACCESS_ENV: &str = "AXOCOATL_VERIFY_STALE_ACCESS_ONCE";

/// The access token the switch sends: a JWT whose payload is
/// `{"exp":1,"iss":"axocoatl-live-verify","sub":"stale-access-token"}`, with
/// no valid signature.
pub const STALE_ACCESS_TOKEN: &str = "eyJhbGciOiJSUzI1NiIsInR5cCI6IkpXVCJ9.\
     eyJleHAiOjEsImlzcyI6ImF4b2NvYXRsLWxpdmUtdmVyaWZ5Iiwic3ViIjoic3RhbGUtYWNjZXNzLXRva2VuIn0.\
     aW52YWxpZC1zaWduYXR1cmU";

/// Not fired yet.
const ARMED: u8 = 0;
/// Sent with one request; the upstream's `401` to it not handled yet.
const SENT: u8 = 1;
/// Spent.
const DONE: u8 = 2;

/// The switch: armed, it replaces the access token of exactly one request,
/// and then lets exactly one `401` to that token refresh the sign-in.
#[derive(Debug)]
pub struct StaleAccessOnce {
    state: AtomicU8,
}

impl StaleAccessOnce {
    pub(crate) fn armed() -> Self {
        Self {
            state: AtomicU8::new(ARMED),
        }
    }

    /// Whether the request now being sent is the one to carry
    /// [`STALE_ACCESS_TOKEN`]: true once, for the first request.
    pub(crate) fn take_first(&self) -> bool {
        self.state
            .compare_exchange(ARMED, SENT, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }

    /// Whether `sent`, a value the upstream answered `401` to, is the one
    /// [`Self::take_first`] sent, its `401` not handled yet: true once.
    pub(crate) fn take_rejection(&self, sent: &str) -> bool {
        sent == STALE_ACCESS_TOKEN
            && self
                .state
                .compare_exchange(SENT, DONE, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
    }
}

/// The switch [`STALE_ACCESS_ENV`]'s value calls for: armed for `1`, none
/// otherwise. Logs at `WARN` when it is armed, and when the variable holds
/// anything else.
pub(crate) fn from_env_value(value: Option<&std::ffi::OsStr>) -> Option<StaleAccessOnce> {
    let value = value?;
    if value != "1" {
        tracing::warn!(
            variable = STALE_ACCESS_ENV,
            "verification switch not turned on: its variable is set to a value other than 1"
        );
        return None;
    }
    tracing::warn!(
        variable = STALE_ACCESS_ENV,
        "VERIFICATION SWITCH ACTIVE (live-verify build): the first request of this process \
         to chatgpt.com carries an invalid access token in place of the stored ChatGPT \
         sign-in's, so that the upstream answers 401 and the broker refreshes the sign-in; this build is \
         for verification only"
    );
    Some(StaleAccessOnce::armed())
}

/// The process's switch: read from [`STALE_ACCESS_ENV`] once, the first
/// time the daemon makes its managed credentials (as it starts), and shared
/// by every broker the process makes after that.
#[cfg(feature = "live-verify")]
pub(crate) fn for_process() -> Option<std::sync::Arc<StaleAccessOnce>> {
    static SWITCH: std::sync::OnceLock<Option<std::sync::Arc<StaleAccessOnce>>> =
        std::sync::OnceLock::new();
    SWITCH
        .get_or_init(|| {
            from_env_value(std::env::var_os(STALE_ACCESS_ENV).as_deref()).map(std::sync::Arc::new)
        })
        .clone()
}
