//! The folder a Session works in, and what happens when that folder is gone.
//!
//! A Session keeps the absolute path of its Workspace folder. A person can
//! delete, move or replace that folder while the daemon keeps the Session.
//! Such a Session must not stop anything else: scans over Sessions (run
//! admission, a new Session, Close or Delete of another Session, reconnect,
//! shutdown) skip it and log once why. Close and Delete of the Session
//! itself succeed, removing only the daemon's own records. Any use of it
//! (a turn, its runtime, Reopen, Ways) is refused with a message that names
//! the folder and how to fix it.
//!
//! The folder is never followed through a symbolic link and never created:
//! it is opened only through [`SecureDir::open`], which refuses a link at
//! the folder itself.
use super::*;

/// Why a Session's folder cannot be used.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum FolderGone {
    /// Nothing is at the path any more (deleted or moved away).
    Missing,
    /// A file, or something else that is not a directory, is at the path.
    NotADirectory,
    /// A symbolic link is at the path. Axocoatl does not follow it.
    SymbolicLink,
}

impl FolderGone {
    fn describe(self, path: &std::path::Path) -> String {
        let path = path.display();
        match self {
            Self::Missing => format!("{path} no longer exists"),
            Self::NotADirectory => format!("{path} is no longer a directory"),
            Self::SymbolicLink => {
                format!("{path} is now a symbolic link, which Axocoatl does not follow")
            }
        }
    }
}

/// Open the Workspace folder at `path`, or say why it is gone.
///
/// The `Err` is any other failure (for example a permission error), which
/// callers keep as an error: such a folder may still hold work.
pub(crate) fn open_workspace_folder(
    path: &std::path::Path,
) -> std::io::Result<Result<SecureDir, FolderGone>> {
    let error = match SecureDir::open(path) {
        Ok(folder) => return Ok(Ok(folder)),
        Err(error) => error,
    };
    // `symlink_metadata` does not follow the folder itself, so a link is
    // reported as a link, never as the directory it points to.
    match std::fs::symlink_metadata(path) {
        Err(metadata_error)
            if matches!(
                metadata_error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            Ok(Err(FolderGone::Missing))
        }
        Ok(metadata) if metadata.file_type().is_symlink() => Ok(Err(FolderGone::SymbolicLink)),
        Ok(metadata) if !metadata.is_dir() => Ok(Err(FolderGone::NotADirectory)),
        _ => Err(error),
    }
}

/// The refusal for using Session `session_id` whose folder `path` is gone.
pub(crate) fn folder_gone_message(
    session_id: &str,
    path: &std::path::Path,
    gone: FolderGone,
) -> String {
    format!(
        "Session {session_id} cannot be used: its Workspace folder {}. Restore the folder at {}, \
         or delete the Session",
        gone.describe(path),
        path.display()
    )
}

/// The refusal for using Session `session_id`, which works in `path`, after
/// its Workspace's folder moved to `now`.
pub(crate) fn folder_moved_message(
    session_id: &str,
    path: &std::path::Path,
    now: &std::path::Path,
) -> String {
    format!(
        "Session {session_id} cannot be used: it works in {}, but its Workspace's folder is now \
         {} (the folder was moved). A Session does not follow a moved folder: move the folder \
         back to {} to continue this Session, or delete the Session and start a new one in {}",
        path.display(),
        now.display(),
        path.display(),
        now.display()
    )
}

impl AxocoatlDaemon {
    /// The folder `session` works in, opened, or a refusal that names it and
    /// says how to fix it: the folder is gone, is a file or a link, or is
    /// no longer its Workspace's folder because the folder moved. Every use
    /// of a Session's folder (its runtime, a turn, Reopen) goes through this
    /// check, so a stale path is never mounted.
    pub(crate) async fn require_session_folder(
        &self,
        session: &Session,
    ) -> Result<SecureDir, DaemonError> {
        if let Some(workspace) = self.get_workspace(&session.workspace_id).await {
            if workspace.canonical_path != session.working_dir {
                return Err(DaemonError::Session(folder_moved_message(
                    &session.id,
                    &session.working_dir,
                    &workspace.canonical_path,
                )));
            }
        }
        match open_workspace_folder(&session.working_dir) {
            Ok(Ok(folder)) => Ok(folder),
            Ok(Err(gone)) => Err(DaemonError::Session(folder_gone_message(
                &session.id,
                &session.working_dir,
                gone,
            ))),
            Err(error) => Err(DaemonError::Session(format!(
                "opening Session {}'s Workspace folder {}: {error}",
                session.id,
                session.working_dir.display()
            ))),
        }
    }

    /// The current attempt set of Session `session_id` for a scan over
    /// Sessions, or for Close and Delete of that Session: a Session whose
    /// folder is gone has no attempt set anything can use, so it reads as
    /// none and the daemon log says why, once per Session and folder.
    /// Every other failure stays an error.
    pub(crate) async fn peek_available_attempt_set(
        &self,
        session_id: &str,
    ) -> Result<Option<crate::git::AttemptSet>, DaemonError> {
        let session = self
            .get_session(session_id)
            .await
            .ok_or_else(|| DaemonError::Session(format!("session '{session_id}' not found")))?;
        self.available_attempt_set_of(&session).await
    }

    /// [`Self::peek_available_attempt_set`] of a Session already read.
    pub(crate) async fn available_attempt_set_of(
        &self,
        session: &Session,
    ) -> Result<Option<crate::git::AttemptSet>, DaemonError> {
        match Self::probe_current_attempt_set_host(&session.working_dir, &session.id).await? {
            Ok(set) => {
                self.forget_unavailable_session_folder(&session.id);
                Ok(set)
            }
            Err(gone) => {
                self.note_unavailable_session_folder(&session.id, &session.working_dir, gone);
                Ok(None)
            }
        }
    }

    /// Log once, per Session and folder, that the folder is gone and the
    /// Session is skipped. The note is dropped when the folder is readable
    /// again, so a later disappearance is logged again.
    pub(crate) fn note_unavailable_session_folder(
        &self,
        session_id: &str,
        path: &std::path::Path,
        gone: FolderGone,
    ) {
        let first = self
            .unavailable_session_folders
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .insert(session_id.to_string(), (path.to_path_buf(), gone))
            != Some((path.to_path_buf(), gone));
        if first {
            tracing::warn!(
                session = %session_id,
                path = %path.display(),
                "the Workspace folder of Session {session_id}: {}; the Session is skipped until \
                 the folder is restored, and can be deleted",
                gone.describe(path)
            );
        }
    }

    /// Session `session_id`'s folder and why it is gone, when it is.
    pub(crate) async fn session_folder_gone(
        &self,
        session_id: &str,
    ) -> Option<(std::path::PathBuf, FolderGone)> {
        let session = self.get_session(session_id).await?;
        match open_workspace_folder(&session.working_dir) {
            Ok(Err(gone)) => Some((session.working_dir, gone)),
            Ok(Ok(_)) | Err(_) => None,
        }
    }

    /// After Close or Delete (`action`) of Session `session_id` succeeded:
    /// say in the log when its folder was already gone, so nothing in it
    /// was stopped or cleaned up.
    pub(crate) fn after_session_folder_lifecycle(
        &self,
        session_id: &str,
        action: &str,
        gone: Option<(std::path::PathBuf, FolderGone)>,
    ) {
        if action == "deleted" {
            self.forget_unavailable_session_folder(session_id);
        }
        if let Some((path, gone)) = gone {
            tracing::warn!(
                session = %session_id,
                path = %path.display(),
                "Session {session_id} was {action}; its Workspace folder {}, so only Axocoatl's \
                 own records of it were changed and nothing in that folder was touched",
                gone.describe(&path)
            );
        }
    }

    fn forget_unavailable_session_folder(&self, session_id: &str) {
        let mut noted = self
            .unavailable_session_folders
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if !noted.is_empty() {
            noted.remove(session_id);
        }
    }
}

#[cfg(all(test, unix))]
#[path = "bootstrap_session_folder_tests.rs"]
mod folder_tests;
