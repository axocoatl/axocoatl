//! Keep as PR on the live daemon. Owner: workstream `keep`.
//!
//! The run must be a finished, passing run of this Session. Its changed
//! paths are the Session's changes attributed to the run's turns: each
//! native turn's activations by their repository captures, each legacy turn
//! by its recorded file-tool writes (the "Last turn" attribution). The
//! result, success or failure, is appended to the run record as
//! `RunEvent::Phase("keep", <KeepResult JSON>)`; the Outcome file itself is
//! written once and never changes.
use super::*;
use crate::git_host::HostTools;
use crate::keep_pr::{
    KeepJob, KeepPrError, KeepPrRequest, KeepPrResponse, KeepRunRecord, RunAttribution,
};
use axocoatl_session::run_outcome::RunOutcome;
use axocoatl_session::run_record::RunRecordStore;
use axocoatl_session::session_history::{HistoryVisibility, SessionHistoryEntry};

/// One Keep at a time: two requests for one run must not race to create
/// its branch or push it twice.
static KEEP_SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

impl AxocoatlDaemon {
    /// `POST /api/sessions/{id}/keep-pr`.
    pub async fn keep_as_pr(
        &self,
        session_id: &str,
        request: KeepPrRequest,
    ) -> Result<KeepPrResponse, DaemonError> {
        crate::keep_pr::validate_request(&request)?;
        if self.get_session(session_id).await.is_none() {
            return Err(DaemonError::Session(format!(
                "session '{session_id}' not found"
            )));
        }
        let store = RunRecordStore::open(&self.data_root).map_err(crate::keep_pr::record_error)?;
        self.keep_as_pr_with(session_id, request, &store, &HostTools::default(), None)
            .await
    }

    /// [`Self::keep_as_pr`] with its run record, host tools and, in tests, a
    /// given attribution (the run's, then the other turns') instead of the
    /// Session history's.
    pub(crate) async fn keep_as_pr_with(
        &self,
        session_id: &str,
        request: KeepPrRequest,
        record: &dyn KeepRunRecord,
        tools: &HostTools,
        attribution: Option<(RunAttribution, RunAttribution)>,
    ) -> Result<KeepPrResponse, DaemonError> {
        crate::keep_pr::validate_request(&request)?;
        let session = self
            .get_session(session_id)
            .await
            .ok_or_else(|| DaemonError::Session(format!("session '{session_id}' not found")))?;
        let _serial = KEEP_SERIAL.lock().await;
        let (manifest, outcome) = crate::keep_pr::load_run(record, session_id, &request.run_id)?;
        let branch = crate::keep_pr::branch_name(&request, &outcome.loadout.id);
        let result = async {
            let (attribution, outside) = match attribution {
                Some(given) => given,
                None => self.keep_run_attribution(&session, &outcome).await?,
            };
            let results = record.keep_results(&request.run_id)?;
            let previous = crate::keep_pr::last_kept_branch(&results).cloned();
            crate::keep_pr::keep(KeepJob {
                session_root: &session.working_dir,
                control_root: &self.data_root,
                request: &request,
                manifest: &manifest,
                outcome: &outcome,
                attribution: &attribution,
                outside: &outside,
                previous: previous.as_ref(),
                tools,
            })
            .await
        }
        .await;
        let recorded = match &result {
            Ok(response) => response.keep_result(),
            Err(error) => error.keep_result(&branch),
        };
        let written = record.record_keep(&request.run_id, &recorded);
        match (result, written) {
            (Ok(response), Ok(())) => Ok(response),
            (Ok(mut response), Err(error)) => {
                tracing::warn!(run = %request.run_id, %error, "Keep result not recorded");
                response.warnings.push(format!(
                    "The branch was kept, but the result could not be added to the run record: {error}"
                ));
                Ok(response)
            }
            (Err(error), _) => Err(error.into()),
        }
    }

    /// The paths the run's turns changed, then the paths the Session's other
    /// turns changed, from this Session's history.
    async fn keep_run_attribution(
        &self,
        session: &Session,
        outcome: &RunOutcome,
    ) -> Result<(RunAttribution, RunAttribution), KeepPrError> {
        if outcome.turns.is_empty() {
            return Err(KeepPrError::Refused(format!(
                "run {} records no turns, so nothing is attributed to it",
                outcome.run_id
            )));
        }
        let history = self
            .versioned_session_history_snapshot(&session.id)
            .await
            .map_err(|error| {
                KeepPrError::Record(format!("the Session's history cannot be read: {error}"))
            })?;
        let mut attribution = RunAttribution::default();
        for turn in &outcome.turns {
            match history.get(&turn.turn_id) {
                Some(SessionHistoryEntry::ExecutionV2(view)) => {
                    attribution.add_execution_turn(view)
                }
                Some(SessionHistoryEntry::LegacyV1(legacy)) => attribution
                    .add_legacy_turn(&legacy.id, rehydrated_turn_touched_paths(session, legacy)),
                None => {
                    return Err(KeepPrError::Refused(format!(
                        "turn {} of run {} is not in this Session's history",
                        turn.turn_id, outcome.run_id
                    )))
                }
            }
        }
        let mut outside = RunAttribution::default();
        for entry in history.entries(HistoryVisibility::IncludingSuperseded) {
            if outcome
                .turns
                .iter()
                .any(|turn| turn.turn_id == entry.turn_id())
            {
                continue;
            }
            match entry {
                SessionHistoryEntry::ExecutionV2(view) => outside.add_execution_turn(view),
                SessionHistoryEntry::LegacyV1(legacy) => outside
                    .add_legacy_turn(&legacy.id, rehydrated_turn_touched_paths(session, legacy)),
            }
        }
        Ok((attribution, outside))
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::keep_pr::tests::MemoryRecord;
    use axocoatl_session::execution_ownership::DataRootFormatOwnership;
    use axocoatl_session::run_outcome::{RunTurnRef, RunVerdict, TurnState};
    use std::os::unix::fs::PermissionsExt;

    fn git(dir: &std::path::Path, script: &str) -> String {
        let output = std::process::Command::new("sh")
            .arg("-c")
            .arg(script)
            .current_dir(dir)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "Setup")
            .env("GIT_AUTHOR_EMAIL", "setup@example.invalid")
            .env("GIT_COMMITTER_NAME", "Setup")
            .env("GIT_COMMITTER_EMAIL", "setup@example.invalid")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{script}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    /// The live daemon checks the Session and the run, attributes through the
    /// Session's history, records every result and keeps a passing run.
    #[tokio::test]
    async fn the_daemon_keeps_a_passing_run_of_the_session_and_records_it() {
        const CHILD: &str = "AXOCOATL_TEST_KEEP_PR_DAEMON_CHILD";
        if std::env::var_os(CHILD).is_some() {
            let mut config = axocoatl_config::AxocoatlConfig::default();
            config.agents.clear();
            config.consolidation.enabled = false;
            let daemon = AxocoatlDaemon::bootstrap_headless(config).await.unwrap();
            let work = std::fs::canonicalize(std::env::var("KEEP_TEST_REPO").unwrap()).unwrap();
            let head = git(&work, "git rev-parse HEAD");
            let workspace = daemon
                .workspace_store
                .lock()
                .await
                .register(&work, Some("Keep"))
                .unwrap();
            let DataRootFormatOwnership::Upgraded(ownership) = &daemon._data_dir_lease.ownership
            else {
                panic!("fresh data roots use native ownership");
            };
            let (session, receipt) = daemon
                .session_store
                .lock()
                .await
                .create_native_with_environment(
                    ownership,
                    "Keep",
                    &workspace.id,
                    &workspace.canonical_path,
                    SessionMode::SingleAgent {
                        agent_id: "conversation".into(),
                    },
                    vec![],
                    vec![],
                    None,
                    None,
                    false,
                    true,
                )
                .unwrap();
            let _token = daemon
                .session_dispatch_lifecycles
                .retain_native_session(ownership.clone(), receipt)
                .unwrap();
            let run_id = "run-0f8c1a2b-3c4d-4e5f-8a9b-0c1d2e3f4a5b";
            let record = MemoryRecord::default();
            let mut outcome = crate::keep_pr::tests::outcome_for(run_id, &session.id);
            let mut manifest = crate::keep_pr::tests::manifest_for(run_id, &session.id);
            manifest.repo_head = Some(head.clone());
            std::thread::sleep(std::time::Duration::from_millis(20));
            manifest.started_at_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64;
            std::fs::write(work.join("a.txt"), "fixed\n").unwrap();
            // The run ends after its change.
            std::thread::sleep(std::time::Duration::from_millis(20));
            outcome.finished_at_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64;
            let request = KeepPrRequest {
                run_id: run_id.into(),
                branch: None,
                open_pr: false,
                remote: None,
                title: None,
            };
            let attributed = RunAttribution {
                paths: ["a.txt".to_string()].into_iter().collect(),
                ..RunAttribution::default()
            };
            let tools = HostTools {
                path: None,
                env: vec![(
                    "GIT_CONFIG_GLOBAL".into(),
                    std::env::var_os("KEEP_TEST_GLOBAL").unwrap(),
                )],
            };
            let keep =
                |request: KeepPrRequest,
                 session: String,
                 attribution: Option<(RunAttribution, RunAttribution)>| {
                    let tools = tools.clone();
                    let record = &record;
                    let daemon = &daemon;
                    async move {
                        daemon
                            .keep_as_pr_with(&session, request, record, &tools, attribution)
                            .await
                    }
                };

            // Validation comes first; an unknown Session is not found.
            let mut invalid = request.clone();
            invalid.branch = Some("-bad".into());
            assert!(matches!(
                keep(invalid, session.id.clone(), None).await,
                Err(DaemonError::InvalidRequest(_))
            ));
            assert!(matches!(
                keep(request.clone(), "ses-00000000-0000-4000-8000-000000000000".into(), None).await,
                Err(DaemonError::Session(message)) if message.contains("not found")
            ));
            // No such run; then a run of another Session; then unfinished.
            assert!(matches!(
                keep(request.clone(), session.id.clone(), None).await,
                Err(DaemonError::SessionConflict(message)) if message.contains("there is no run")
            ));
            let mut foreign = manifest.clone();
            foreign.session_id = "ses-00000000-0000-4000-8000-000000000001".into();
            record
                .manifests
                .lock()
                .unwrap()
                .insert(run_id.into(), foreign);
            assert!(matches!(
                keep(request.clone(), session.id.clone(), None).await,
                Err(DaemonError::SessionConflict(message)) if message.contains("is not a run of Session")
            ));
            record
                .manifests
                .lock()
                .unwrap()
                .insert(run_id.into(), manifest.clone());
            assert!(matches!(
                keep(request.clone(), session.id.clone(), None).await,
                Err(DaemonError::SessionConflict(message)) if message.contains("has not finished")
            ));
            // Not passed.
            let mut attention = outcome.clone();
            attention.verdict = RunVerdict::NeedsAttention;
            attention.exit_code = 2;
            record
                .outcomes
                .lock()
                .unwrap()
                .insert(run_id.into(), attention);
            assert!(matches!(
                keep(request.clone(), session.id.clone(), None).await,
                Err(DaemonError::SessionConflict(message)) if message.contains("did not pass")
            ));
            // A turn the Session's history does not hold is refused and recorded.
            outcome.turns = vec![RunTurnRef {
                turn_id: "turn-missing".into(),
                purpose: "run".into(),
                state: TurnState::Completed,
            }];
            record
                .outcomes
                .lock()
                .unwrap()
                .insert(run_id.into(), outcome.clone());
            assert!(matches!(
                keep(request.clone(), session.id.clone(), None).await,
                Err(DaemonError::SessionConflict(message)) if message.contains("not in this Session's history")
            ));
            assert_eq!(record.keeps.lock().unwrap().len(), 1);
            assert!(record.keeps.lock().unwrap()[0].1.error.is_some());
            // A passing run with its attribution is kept, and recorded.
            let kept = keep(
                request.clone(),
                session.id.clone(),
                Some((attributed.clone(), RunAttribution::default())),
            )
            .await
            .unwrap();
            assert_eq!(kept.paths, vec!["a.txt"]);
            assert_eq!(
                git(&work, "git rev-parse axocoatl/fix-0f8c1a2b"),
                kept.commit
            );
            assert_eq!(git(&work, "git rev-parse HEAD"), head);
            let keeps = record.keeps.lock().unwrap().clone();
            assert_eq!(keeps.len(), 2);
            assert_eq!(keeps[1].1, kept.keep_result());
            // Keeping again continues from the recorded branch.
            let again = keep(
                request.clone(),
                session.id.clone(),
                Some((attributed, RunAttribution::default())),
            )
            .await
            .unwrap();
            assert_eq!(again.commit, kept.commit);
            daemon.shutdown().await.unwrap();
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let root_path = std::fs::canonicalize(root.path()).unwrap();
        let repo = root_path.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        git(
            &repo,
            "git init -q -b work && printf one > a.txt && git add a.txt && git commit -q -m initial",
        );
        let global = root_path.join("gitconfig");
        std::fs::write(
            &global,
            "[user]\n\tname = Person\n\temail = person@example.invalid\n",
        )
        .unwrap();
        let bin = root_path.join("bin");
        std::fs::create_dir(&bin).unwrap();
        let podman = bin.join("podman");
        std::fs::write(
            &podman,
            r#"#!/bin/sh
case "$*" in
  --version) printf 'podman version 5.0.0\n' ;;
  'machine list --format json') printf '[{"Running":true}]\n' ;;
  'info --format json') printf '{}\n' ;;
  'ps '*) ;;
  *) printf 'unexpected Podman command: %s\n' "$*" >&2; exit 1 ;;
esac
"#,
        )
        .unwrap();
        std::fs::set_permissions(&podman, std::fs::Permissions::from_mode(0o700)).unwrap();
        let git_dir = std::path::Path::new(&git(&root_path, "command -v git"))
            .parent()
            .unwrap()
            .to_path_buf();
        let path = format!("{}:{}:/usr/bin:/bin", bin.display(), git_dir.display());
        let result = tokio::time::timeout(
            Duration::from_secs(120),
            tokio::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "bootstrap::keep_pr_host::tests::the_daemon_keeps_a_passing_run_of_the_session_and_records_it",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .env("AXOCOATL_DATA_DIR", root_path.join("data"))
                .env("AXOCOATL_SOCKET_PATH", root_path.join("ipc/daemon.sock"))
                .env("KEEP_TEST_REPO", &repo)
                .env("KEEP_TEST_GLOBAL", &global)
                .env("PATH", path)
                .current_dir(&root_path)
                .kill_on_drop(true)
                .output(),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            result.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
    }
}
