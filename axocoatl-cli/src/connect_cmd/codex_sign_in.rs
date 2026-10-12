//! `axocoatl connect codex --refresh` and `axocoatl connect codex --status`:
//! the stored ChatGPT sign-in, without a token.
//!
//! `--refresh` asks the running daemon (`POST /api/connect/codex/refresh`,
//! with the local API token, the way `axocoatl run` talks to it) to refresh
//! the sign-in now. The daemon does it with the broker that refreshes it
//! before a request (`codex_chatgpt::ChatgptBroker::refresh_now`): the same
//! request to OpenAI, rotation, atomic write under the secret store's lock,
//! and remembered refusal, serialized with the route's own refreshes, so
//! the command never sends a refresh token itself and never races the
//! daemon. It prints when the new access token expires and when it was
//! refreshed. Exit codes: [`exit::CONNECTED`] refreshed,
//! [`exit::REJECTED`] OpenAI refused the refresh (sign in again),
//! [`exit::NO_TOKEN`] no sign-in stored, [`exit::UNVERIFIED`] the refresh
//! could not be completed, [`exit::DAEMON_NOT_RUNNING`] no daemon answers,
//! [`exit::USAGE`] a bad `--url`, [`exit::FAILURE`] anything else.
//!
//! `--status` reads the stored sign-in from the data root (no daemon
//! needed): whether one is stored, its ChatGPT account id masked to its
//! last four characters, when its access token expires (its JWT `exp`) and
//! when its tokens were obtained (the last refresh, or the sign-in when
//! there has been none since).

use std::path::Path;
use std::time::Duration;

use axocoatl_daemon::external_agent::codex_chatgpt::{
    read_stored, ChatgptSignIn, RefreshReport, CODEX_CHATGPT_SECRET,
};

use super::{exit, failure, Failure};

/// The daemon API route `--refresh` calls.
pub(crate) const REFRESH_PATH: &str = "/api/connect/codex/refresh";
/// How long `--refresh` waits for the daemon's answer: a refresh waits for
/// another one under way and for the secret store's lock, and OpenAI gets
/// 30 seconds.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(120);

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

/// `ms` (milliseconds since the Unix epoch) in this computer's time zone,
/// with its offset.
pub(crate) fn local_time(ms: u64) -> String {
    i64::try_from(ms)
        .ok()
        .and_then(chrono::DateTime::from_timestamp_millis)
        .map(|time| {
            time.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M:%S %:z")
                .to_string()
        })
        .unwrap_or_else(|| format!("{ms} ms after the Unix epoch"))
}

/// How far `ms` is from `now_ms`, in words: "in 59 minutes", "3 days ago".
pub(crate) fn relative(ms: u64, now_ms: u64) -> String {
    let (seconds, future) = if ms >= now_ms {
        ((ms - now_ms) / 1000, true)
    } else {
        ((now_ms - ms) / 1000, false)
    };
    let (count, unit) = match seconds {
        0..=59 => (seconds, "second"),
        60..=3599 => (seconds / 60, "minute"),
        3600..=172_799 => (seconds / 3600, "hour"),
        _ => (seconds / 86_400, "day"),
    };
    let amount = format!("{count} {unit}{}", if count == 1 { "" } else { "s" });
    if future {
        format!("in {amount}")
    } else {
        format!("{amount} ago")
    }
}

/// `ms` in local time and relative to `now_ms`.
fn when(ms: u64, now_ms: u64) -> String {
    format!("{} ({})", local_time(ms), relative(ms, now_ms))
}

/// A ChatGPT account id shown by its last four characters only.
pub(crate) fn masked_account_id(id: &str) -> String {
    let count = id.chars().count();
    if count <= 4 {
        return "(hidden: it has 4 characters or fewer)".to_string();
    }
    let last: String = id.chars().skip(count - 4).collect();
    format!("…{last}")
}

/// The line on the new access token's expiry.
fn expiry_line(expires_at_ms: Option<u64>, now_ms: u64) -> String {
    match expires_at_ms {
        Some(expires) => format!("The new access token expires at {}.", when(expires, now_ms)),
        None => "The new access token carries no expiry time; the daemon tries to refresh it \
                 before the first request made more than eight days after it was obtained."
            .to_string(),
    }
}

/// What the daemon's report comes to: the line to print (standard output)
/// or the failure, with its exit code.
pub(crate) fn refresh_outcome(report: RefreshReport, now_ms: u64) -> Result<String, Failure> {
    match report {
        RefreshReport::Refreshed {
            refreshed_at_ms,
            expires_at_ms,
        } => Ok(format!(
            "Refreshed the ChatGPT sign-in at {} and stored it as secret \
             {CODEX_CHATGPT_SECRET} (no token was shown). {}",
            local_time(refreshed_at_ms),
            expiry_line(expires_at_ms, now_ms)
        )),
        RefreshReport::Replaced {
            refreshed_at_ms,
            expires_at_ms,
        } => Ok(format!(
            "OpenAI gave new tokens, but a sign-in stored with `axocoatl connect codex` while \
             the refresh ran is kept instead (it was obtained at {}; {}).",
            local_time(refreshed_at_ms),
            match expires_at_ms {
                Some(expires) => format!("its access token expires at {}", when(expires, now_ms)),
                None => "its access token carries no expiry time".to_string(),
            }
        )),
        RefreshReport::NotStored {
            refreshed_at_ms,
            expires_at_ms,
            message,
        } => Err(failure(
            exit::FAILURE,
            format!(
                "Refreshed the ChatGPT sign-in at {}, but {message}. {}",
                local_time(refreshed_at_ms),
                expiry_line(expires_at_ms, now_ms)
            ),
        )),
        RefreshReport::NoSignIn { message } => Err(failure(exit::NO_TOKEN, message)),
        RefreshReport::Refused { message, .. } => Err(failure(exit::REJECTED, message)),
        RefreshReport::Failed { message } => Err(failure(exit::UNVERIFIED, message)),
        RefreshReport::Unusable { message } | RefreshReport::Unavailable { message } => {
            Err(failure(exit::FAILURE, message))
        }
    }
}

/// Ask the daemon at `daemon` to refresh the sign-in now.
pub(crate) async fn refresh(daemon: &crate::run_cmd::Daemon) -> Result<String, Failure> {
    let response = daemon
        .request(reqwest::Method::POST, REFRESH_PATH)
        .timeout(ANSWER_TIMEOUT)
        .send()
        .await
        .map_err(|error| {
            if error.is_connect() {
                failure(
                    exit::DAEMON_NOT_RUNNING,
                    format!(
                        "no Axocoatl daemon answers at {}, so the sign-in was not refreshed. \
                         Start it with `axocoatl serve` (it refreshes the sign-in itself before \
                         a request), or pass --url",
                        daemon.base()
                    ),
                )
            } else if error.is_timeout() {
                failure(
                    exit::FAILURE,
                    format!(
                        "the daemon at {} did not answer within {} seconds; the daemon log says \
                         whether the refresh finished",
                        daemon.base(),
                        ANSWER_TIMEOUT.as_secs()
                    ),
                )
            } else {
                failure(
                    exit::FAILURE,
                    format!("asking the daemon at {} failed: {error}", daemon.base()),
                )
            }
        })?;
    let status = response.status();
    let text = response.text().await.map_err(|error| {
        failure(
            exit::FAILURE,
            format!("reading the daemon's answer failed: {error}"),
        )
    })?;
    if let Ok(report) = serde_json::from_str::<RefreshReport>(&text) {
        return refresh_outcome(report, now_ms());
    }
    let said = serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|value| value.get("error")?.as_str().map(str::to_string));
    Err(failure(
        exit::FAILURE,
        match status.as_u16() {
            401 => "the daemon refused the local API token (401); run the command as the user \
                    the daemon runs as, with its configuration (-c), or set AXOCOATL_TOKEN"
                .to_string(),
            404 => "this daemon does not refresh the ChatGPT sign-in through its API (it is older \
                    than Axocoatl 1.3.1); restart it with this version"
                .to_string(),
            _ => match said {
                Some(said) => format!("the daemon refused ({status}): {said}"),
                None => format!("the daemon answered {status}"),
            },
        },
    ))
}

/// `axocoatl connect codex --refresh`. Returns the process exit code.
pub(crate) async fn cmd_refresh(url: Option<&str>, config: &Path) -> i32 {
    let daemon = match crate::run_cmd::Daemon::locate(url, config).await {
        Ok(daemon) => daemon,
        Err(located) => {
            let code = if located.code == axocoatl_session::run_outcome::exit_code::USAGE {
                exit::USAGE
            } else {
                exit::FAILURE
            };
            eprintln!("✗ {}", located.message);
            return code;
        }
    };
    match refresh(&daemon).await {
        Ok(message) => {
            println!("✓ {message}");
            exit::CONNECTED
        }
        Err(Failure { code, message }) => {
            eprintln!("✗ {message}");
            code
        }
    }
}

/// What `--status` prints for `stored` under `data_dir`.
pub(crate) fn status_text(
    stored: Result<Option<ChatgptSignIn>, String>,
    data_dir: &Path,
    now_ms: u64,
) -> Result<String, Failure> {
    let sign_in = match stored {
        Ok(Some(sign_in)) => sign_in,
        Ok(None) => {
            return Err(failure(
                exit::NO_TOKEN,
                format!(
                    "No ChatGPT sign-in is stored (secret {CODEX_CHATGPT_SECRET} in {}). Sign \
                     in with `axocoatl connect codex`.",
                    data_dir.display()
                ),
            ))
        }
        Err(reason) => return Err(failure(exit::FAILURE, reason)),
    };
    let account = sign_in
        .account_id()
        .map_or_else(|| "none stored".to_string(), masked_account_id);
    let expiry = match sign_in.expires_at_ms() {
        Some(expires) if expires > now_ms => format!("expires at {}", when(expires, now_ms)),
        Some(expires) => format!(
            "expired at {}; the daemon tries to refresh it before the next request",
            when(expires, now_ms)
        ),
        None => "carries no expiry time; the daemon tries to refresh it before the first request \
                 made more than eight days after it was obtained"
            .to_string(),
    };
    Ok(format!(
        "A ChatGPT sign-in is stored (secret {CODEX_CHATGPT_SECRET} in {}); no token is shown.\n  \
         Account: {account}\n  Access token: {expiry}\n  Last refreshed or signed in: {}",
        data_dir.display(),
        when(sign_in.refreshed_at_ms(), now_ms),
    ))
}

/// `axocoatl connect codex --status`. Returns the process exit code.
pub(crate) fn cmd_status(config: &Path) -> i32 {
    let data_dir = match crate::configure_data_dir(config) {
        Ok(data_dir) => data_dir,
        Err(error) => {
            eprintln!("✗ {error}");
            return exit::FAILURE;
        }
    };
    match status_text(read_stored(&data_dir), &data_dir, now_ms()) {
        Ok(text) => {
            println!("{text}");
            exit::CONNECTED
        }
        Err(Failure { code, message }) => {
            eprintln!("✗ {message}");
            code
        }
    }
}

#[cfg(test)]
#[path = "codex_sign_in_tests.rs"]
mod tests;
