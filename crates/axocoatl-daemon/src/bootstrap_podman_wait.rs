//! Asking a slow Podman again while the daemon proves, before it starts (or
//! before its first local Session after a start without Podman), that no
//! earlier Session container still exposes Axocoatl's directories and that
//! the containers of its own earlier Sessions are gone.
//!
//! That proof fails closed: without an answer from Podman the daemon does
//! not start. But a Podman that is only slow answers when asked again. Each
//! `podman` command keeps its own bound (`podman ps` 10 s, the readiness
//! probes 15 s); a command that did not answer within it
//! ([`IsolationError::PodmanNotAnswering`]) is asked again until
//! [`PODMAN_ANSWER_WAIT`] has passed since the proof began. Any other error
//! is final at once.

use std::future::Future;
use std::time::Duration;

use axocoatl_isolation::IsolationError;

use crate::DaemonError;

/// How long the proof keeps asking a Podman that does not answer. Podman
/// 4.9.3 (Ubuntu 24.04's) holds `podman ps` until another Podman command
/// that deletes a container's files ends: removing one whose writable layer
/// held 600,000 files kept `podman ps` waiting 69 s on an otherwise idle
/// 8-CPU VM. Podman 6.1.4 in a macOS machine answered `podman ps` within
/// 4.4 s while an image loaded and containers came and went. A start that
/// gives up after this wait and one more attempt (at most about a minute)
/// still says why before onboarding stops waiting for it (300 s).
pub(crate) const PODMAN_ANSWER_WAIT: Duration = Duration::from_secs(180);
/// The pause before asking again.
const ASK_AGAIN_AFTER: Duration = Duration::from_secs(1);

/// One proof's patience: the time it began and how long it may wait.
pub(crate) struct PodmanPatience {
    started: tokio::time::Instant,
    wait: Duration,
}

impl PodmanPatience {
    pub(crate) fn new(wait: Duration) -> Self {
        Self {
            started: tokio::time::Instant::now(),
            wait,
        }
    }

    /// Run `call` until Podman answers it, asking again while Podman does
    /// not answer in time and this proof's wait has not passed.
    pub(crate) async fn ask<T, F, Fut>(&self, mut call: F) -> Result<T, IsolationError>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<T, IsolationError>>,
    {
        loop {
            match call().await {
                Err(error)
                    if error.is_podman_not_answering() && self.started.elapsed() < self.wait =>
                {
                    tracing::warn!(
                        %error,
                        waited_s = self.started.elapsed().as_secs(),
                        wait_s = self.wait.as_secs(),
                        "Podman did not answer in time; asking again before Axocoatl goes on"
                    );
                    tokio::time::sleep(ASK_AGAIN_AFTER).await;
                }
                other => return other,
            }
        }
    }

    /// The error that ends the proof: when Podman did not answer, how long
    /// it was asked and what to do; otherwise `context` and `error`.
    pub(crate) fn failure(
        &self,
        context: impl FnOnce() -> String,
        error: IsolationError,
    ) -> DaemonError {
        if error.is_podman_not_answering() {
            return DaemonError::Session(format!(
                "Podman did not answer for {} s ({error}), and Axocoatl goes on only once \
                 Podman has said which Session containers an earlier start left. Podman may \
                 still be starting, or another Podman command (removing a large container or \
                 image, say) may be holding it. Check that `podman ps` answers, then try again.",
                self.started.elapsed().as_secs()
            ));
        }
        DaemonError::Session(format!("{}: {error}", context()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn not_answering() -> IsolationError {
        IsolationError::podman_not_answering("ps", Duration::from_secs(10))
    }

    /// A Podman that does not answer at first is asked again until it does.
    #[tokio::test(start_paused = true)]
    async fn a_slow_podman_is_asked_again_until_it_answers() {
        let patience = PodmanPatience::new(PODMAN_ANSWER_WAIT);
        let calls = AtomicUsize::new(0);
        let answer = patience
            .ask(|| {
                let call = calls.fetch_add(1, Ordering::SeqCst);
                async move {
                    if call < 3 {
                        tokio::time::sleep(Duration::from_secs(10)).await;
                        Err(not_answering())
                    } else {
                        Ok("answered")
                    }
                }
            })
            .await
            .unwrap();
        assert_eq!(answer, "answered");
        assert_eq!(calls.load(Ordering::SeqCst), 4);
    }

    /// One that never answers is asked until the wait has passed, and the
    /// error says so, with the command and what to do.
    #[tokio::test(start_paused = true)]
    async fn a_podman_that_never_answers_ends_the_wait_and_says_what_to_do() {
        let patience = PodmanPatience::new(PODMAN_ANSWER_WAIT);
        let calls = AtomicUsize::new(0);
        let error = patience
            .ask(|| {
                calls.fetch_add(1, Ordering::SeqCst);
                async {
                    tokio::time::sleep(Duration::from_secs(10)).await;
                    Err::<(), _>(not_answering())
                }
            })
            .await
            .unwrap_err();
        let waited = patience.started.elapsed();
        assert!(waited >= PODMAN_ANSWER_WAIT, "{waited:?}");
        assert!(
            waited < PODMAN_ANSWER_WAIT + Duration::from_secs(12),
            "{waited:?}"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 17);
        let message = patience
            .failure(|| "never shown".to_string(), error)
            .to_string();
        assert!(
            message.contains(
                "Podman did not answer for 186 s (Podman did not answer `podman ps` within 10 s)"
            ),
            "{message}"
        );
        assert!(
            message.contains("Check that `podman ps` answers, then try again."),
            "{message}"
        );
        assert!(!message.contains("never shown"), "{message}");
        assert!(!message.contains("Tool execution"), "{message}");
    }

    /// Any other error is final at once and keeps its context.
    #[tokio::test(start_paused = true)]
    async fn a_podman_that_answers_with_an_error_is_not_asked_again() {
        let patience = PodmanPatience::new(PODMAN_ANSWER_WAIT);
        let calls = AtomicUsize::new(0);
        let error = patience
            .ask(|| {
                calls.fetch_add(1, Ordering::SeqCst);
                async {
                    Err::<(), _>(IsolationError::OciContainerFailed(
                        "listing legacy Axocoatl containers: connection refused".into(),
                    ))
                }
            })
            .await
            .unwrap_err();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let message = patience
            .failure(
                || "early bootstrap could not prove X absent".to_string(),
                error,
            )
            .to_string();
        assert!(
            message.contains("early bootstrap could not prove X absent: OCI container execution failed: listing legacy Axocoatl containers: connection refused"),
            "{message}"
        );
    }
}
