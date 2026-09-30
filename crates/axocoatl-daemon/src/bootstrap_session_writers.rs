//! Participating short writers retain the Workspace operation through the
//! actual owned executor future, even when their HTTP waiter disappears.
//!
//! This is task-lifetime fencing, not proof of process-tree settlement. An
//! ordinary exec result cannot establish that arbitrary Git hooks left no
//! descendants. Interactive terminals/background tasks require separate
//! process-boundary integration and do not acquire these short-operation leases.

use super::{require_session_environment_ready, session_git_arguments, AxocoatlDaemon};
use crate::error::DaemonError;
use axocoatl_isolation::session_sandbox::{ExecResult, Sandbox};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::OwnedMutexGuard;

type Result<T> = std::result::Result<T, DaemonError>;

/// Clones belong to one already-admitted operation. Passing this explicitly
/// prevents unrelated requests from borrowing another writer's authority.
pub(super) struct SessionWriterLease {
    session_id: String,
    operation: Arc<OwnedMutexGuard<()>>,
}

impl SessionWriterLease {
    pub(super) async fn exec(
        &self,
        sandbox: Arc<dyn Sandbox>,
        argv: Vec<String>,
        stdin: Option<String>,
        timeout: Duration,
    ) -> Result<ExecResult> {
        self.run(async move {
            let argv = argv.iter().map(String::as_str).collect::<Vec<_>>();
            match stdin {
                Some(stdin) => sandbox.exec_stdin(&argv, &stdin, timeout).await,
                None => sandbox.exec(&argv, timeout).await,
            }
            .map_err(|error| DaemonError::Session(error.to_string()))
        })
        .await
    }

    async fn run<T, F>(&self, operation: F) -> Result<T>
    where
        T: Send + 'static,
        F: std::future::Future<Output = Result<T>> + Send + 'static,
    {
        let lease = self.operation.clone();
        // The task exists before the first yield. Dropping this receiver does
        // not abort it, so the last Workspace lease follows the executor's
        // future rather than the lifetime of an HTTP connection.
        tokio::spawn(async move {
            let result = operation.await;
            drop(lease);
            result
        })
        .await
        .map_err(|error| {
            DaemonError::Session(format!(
                "Session writer task did not complete normally: {error}"
            ))
        })?
    }
}

impl AxocoatlDaemon {
    pub(super) async fn session_writer(&self, session_id: &str) -> Result<SessionWriterLease> {
        self.require_runtime_admission()?;
        let operation = self.attempt_operation(session_id).await.lock_owned().await;
        self.require_runtime_admission()?;
        self.require_no_unresolved_attempt(session_id).await?;
        let session = self
            .get_session(session_id)
            .await
            .ok_or_else(|| DaemonError::Session(format!("unknown Session {session_id}")))?;
        require_session_environment_ready(&session)?;
        Ok(SessionWriterLease {
            session_id: session.id,
            operation: Arc::new(operation),
        })
    }

    pub(super) async fn session_git_with_writer(
        &self,
        writer: &SessionWriterLease,
        session_id: &str,
        args: &[&str],
    ) -> Result<ExecResult> {
        if writer.session_id != session_id {
            return Err(DaemonError::SessionConflict(
                "writer lease belongs to another Session".into(),
            ));
        }
        let session = self
            .get_session(session_id)
            .await
            .ok_or_else(|| DaemonError::Session(format!("unknown Session {session_id}")))?;
        let sandbox = self.ensure_sandbox(&session).await?;
        let argv = session_git_arguments(&sandbox.root().to_string_lossy(), args);
        writer
            .exec(sandbox, argv, None, Duration::from_secs(60))
            .await
    }

    /// Initializing a repository is a write even when requested by Status.
    /// The ordinary read probe remains available without owning the writer gate.
    pub(super) async fn ensure_session_git_with_writer(
        &self,
        session_id: &str,
        writer: &SessionWriterLease,
    ) -> Result<()> {
        let probe = self
            .session_git(session_id, &["rev-parse", "--is-inside-work-tree"])
            .await?;
        if probe.ok() && probe.stdout.trim() == "true" {
            return Ok(());
        }
        for args in [
            vec!["init", "-q"],
            vec!["config", "user.email", "agent@axocoatl.local"],
            vec!["config", "user.name", "Axocoatl"],
            vec!["add", "-A"],
            vec!["commit", "-q", "-m", "axocoatl: baseline", "--allow-empty"],
        ] {
            let result = self
                .session_git_with_writer(writer, session_id, &args)
                .await?;
            if !result.ok() {
                return Err(DaemonError::Session(format!(
                    "initializing Session Git repository: {}",
                    result.stderr.trim()
                )));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::sync::{oneshot, Mutex};

    #[tokio::test]
    async fn disconnected_writer_waiter_keeps_workspace_excluded_until_executor_returns() {
        let gate = Arc::new(Mutex::new(()));
        let writer = SessionWriterLease {
            session_id: "session-a".into(),
            operation: Arc::new(gate.clone().lock_owned().await),
        };
        let (started, ready) = oneshot::channel();
        let (finish, finished) = oneshot::channel();
        let completed = Arc::new(AtomicBool::new(false));
        let observed = completed.clone();
        let waiter = tokio::spawn(async move {
            writer
                .run(async move {
                    started.send(()).unwrap();
                    finished.await.unwrap();
                    observed.store(true, Ordering::SeqCst);
                    Ok(())
                })
                .await
        });
        ready.await.unwrap();
        waiter.abort();
        assert!(waiter.await.unwrap_err().is_cancelled());
        assert!(gate.clone().try_lock_owned().is_err());
        assert!(!completed.load(Ordering::SeqCst));
        finish.send(()).unwrap();
        let _released = tokio::time::timeout(Duration::from_secs(2), gate.lock_owned())
            .await
            .unwrap();
        assert!(completed.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn multiple_steps_share_one_operation_without_unlocking_between_them() {
        let gate = Arc::new(Mutex::new(()));
        let writer = SessionWriterLease {
            session_id: "session-a".into(),
            operation: Arc::new(gate.clone().lock_owned().await),
        };
        assert_eq!(writer.run(async { Ok(7) }).await.unwrap(), 7);
        assert!(gate.clone().try_lock_owned().is_err());
        let error = writer
            .run(async { Err::<(), _>(DaemonError::Session("executor failure".into())) })
            .await
            .unwrap_err();
        assert!(error.to_string().contains("executor failure"));
        assert!(gate.clone().try_lock_owned().is_err());
        drop(writer);
        assert!(gate.try_lock_owned().is_ok());
    }
}
