//! Existing Session lifecycle with canonical history retained across physical
//! runtime teardown. Reopening history does not recreate execution ownership.
use super::*;
use axocoatl_session::execution_ownership::DataRootFormatOwnership;

/// Who holds a Workspace, as its open Sessions show it.
#[derive(Debug, Default)]
pub(super) struct WorkspaceHolders {
    /// One entry per open Session of the Workspace whose turn is running or
    /// needs a person: each such turn holds the Workspace until it ends.
    pub(super) turns: Vec<String>,
    /// Every open Session of the Workspace, with its loadout run.
    pub(super) open: Vec<String>,
}

impl WorkspaceHolders {
    /// `is held by …` for a refusal: the open turns when there are any,
    /// otherwise the open Sessions.
    pub(super) fn held_by(&self) -> String {
        if !self.turns.is_empty() {
            format!("is held by {}", self.turns.join("; "))
        } else if !self.open.is_empty() {
            format!(
                "is held by another operation of one of its open Sessions: {}",
                self.open.join(", ")
            )
        } else {
            "is held by another operation".to_string()
        }
    }
}

impl AxocoatlDaemon {
    /// The open Sessions of `workspace_id` (all but `except`) and those
    /// among them whose turn is running or needs a person, named with their
    /// loadout run and turn.
    pub(super) async fn workspace_holders(
        &self,
        workspace_id: &str,
        except: Option<&str>,
    ) -> WorkspaceHolders {
        let mut holders = WorkspaceHolders::default();
        for session in self.list_sessions().await {
            if session.workspace_id != workspace_id
                || session.status == axocoatl_session::SessionStatus::Closed
                || except == Some(session.id.as_str())
            {
                continue;
            }
            let run = session
                .loadout
                .as_ref()
                .map(|binding| format!(" (loadout run {})", binding.run_id))
                .unwrap_or_default();
            holders.open.push(format!("{}{run}", session.id));
            let open_turn = self.open_turn_of(&session.id).await;
            if let Some(turn) = open_turn {
                holders
                    .turns
                    .push(format!("Session {}{run}, whose {turn}", session.id));
            }
        }
        holders
    }

    /// After Close stopped a local Session's containers: remove the volumes
    /// its runtime fills again whenever it starts (the egress proxy's socket,
    /// identity-socket and service-socket volumes and the trust volume), so a
    /// closed Session leaves none behind. The Node dependency volume stays,
    /// so Reopen reuses the installed dependencies; Delete removes it. A
    /// volume Podman cannot remove (another container still uses it) does
    /// not undo the Close; the daemon log names it.
    pub(super) async fn remove_closed_session_runtime_volumes(&self, id: &str) {
        let Some(session) = self.get_session(id).await else {
            return;
        };
        if !self.local_runtime_repair(&session) {
            return;
        }
        if let Err(error) = SessionSandbox::remove_runtime_volumes(id).await {
            tracing::warn!(
                session = %id,
                %error,
                "a closed Session's runtime volumes were not all removed"
            );
        }
    }

    pub(super) fn uses_native_session_history(&self) -> bool {
        matches!(
            self._data_dir_lease.ownership,
            DataRootFormatOwnership::Upgraded(_)
        )
    }
    pub(super) fn restore_native_lifecycle_history(
        &self,
        session: &Session,
        closed: bool,
    ) -> Result<(), DaemonError> {
        let DataRootFormatOwnership::Upgraded(ownership) = &self._data_dir_lease.ownership else {
            return Ok(());
        };
        self._data_dir_lease
            .ownership
            .verify_root(&self.data_root)
            .map_err(|error| DaemonError::Session(error.to_string()))?;
        if self
            .session_dispatch_lifecycles
            .retains_session(&session.id)?
        {
            return Ok(());
        }
        let stores = session_recovery::recover_session_stores(ownership.clone(), session)?;
        self.session_dispatch_lifecycles
            .retain_existing_lifecycle_session(&mut Some(stores), closed)?;
        Ok(())
    }
    /// Repair: prepare the exact approved runtime again at its current
    /// generation for a Session whose paused turn is bound to that generation.
    /// This is an explicit lifecycle operation; nothing about the plan changes
    /// and ordinary Files, Git, Terminal or turn requests never repair.
    async fn retry_paused_session_environment(&self, id: &str) -> Result<Session, DaemonError> {
        self.require_runtime_admission()?;
        // Hold the Workspace operation gate first: no attach or Begin can take
        // a repository owner for this Workspace while the repair runs. A
        // held Workspace refuses the repair at once, naming its holder.
        let _operation = self
            .take_session_lifecycle_operation(
                id,
                workspace_operation::WorkspaceRequest {
                    doing: format!("the repair of Session {id}'s environment"),
                    refused: format!("The environment of Session {id} was not repaired"),
                },
            )
            .await?;
        let _start = self
            .lock_session_start_for_lifecycle(
                id,
                &format!("The environment of Session {id} was not repaired"),
            )
            .await?;
        self.require_runtime_admission()?;
        self.require_no_unresolved_attempt(id).await?;
        self.session_dispatch_lifecycles
            .require_native_environment_retry_ready(id)?;
        let latest = self
            .get_session(id)
            .await
            .ok_or_else(|| DaemonError::Session(format!("session '{id}' not found")))?;
        if latest.status == axocoatl_session::SessionStatus::Closed {
            return Err(DaemonError::SessionConflict(
                "Reopen this Session before changing its runtime".into(),
            ));
        }
        if latest.environment.state != SessionEnvironmentState::Failed {
            return Ok(latest);
        }
        if !self.local_runtime_repair(&latest) {
            return Err(DaemonError::SessionConflict(
                "Stop or Finish the unfinished Session turn before rebuilding this runtime".into(),
            ));
        }
        if self.session_sandboxes.lock().await.contains_key(id) {
            return Err(DaemonError::SessionConflict(
                "A runtime is still attached to this Session; Stop or Finish the unfinished Session turn first"
                    .into(),
            ));
        }
        let _environment_change = self
            .stream_bus
            .begin_session_environment_change(id, latest.environment.generation);
        // A failed attempt may leave a container or a partial dependency
        // volume; neither is trusted. The generation is not changed.
        self.remove_session_sandbox_with_dependencies_checked(id, true)
            .await?;
        let latest = self
            .get_session(id)
            .await
            .ok_or_else(|| DaemonError::Session(format!("session '{id}' not found")))?;
        self.prepare_session_environment(&latest).await
    }

    /// Only a local runtime is recreated from its approved plan with the
    /// paused turn's workspace on the host; a remote runtime holds that
    /// workspace itself and is never replaced under a paused turn.
    fn local_runtime_repair(&self, session: &Session) -> bool {
        self.config.sandbox.backend != "e2b"
            && session
                .environment
                .runtime
                .as_ref()
                .is_none_or(|runtime| runtime.backend == "podman")
    }

    pub(super) async fn configure_native_session_environment(
        &self,
        id: &str,
        image: Option<String>,
        setup_command: Option<String>,
        setup_approved: bool,
        setup_reviewed: bool,
    ) -> Result<Session, DaemonError> {
        self.require_runtime_admission()?;
        let current = self
            .get_session(id)
            .await
            .ok_or_else(|| DaemonError::Session(format!("session '{id}' not found")))?;
        if current.status == axocoatl_session::SessionStatus::Closed {
            return Err(DaemonError::SessionConflict(
                "Reopen this Session before changing its runtime".into(),
            ));
        }
        // Rebuilding the unchanged plan of a failed environment while a paused
        // turn waits keeps the generation that turn is bound to.
        if current.environment.state == SessionEnvironmentState::Failed
            && self.local_runtime_repair(&current)
            && current.environment_plan_matches(
                image.as_deref(),
                setup_command.as_deref(),
                setup_approved,
                setup_reviewed,
            )
            && self
                .session_dispatch_lifecycles
                .require_native_environment_change_ready(id)
                .is_err()
        {
            return self.retry_paused_session_environment(id).await;
        }
        self.session_dispatch_lifecycles
            .require_native_environment_change_ready(id)?;
        self.require_no_unresolved_attempt(id).await?;
        // Take the Workspace before anything changes, without waiting for
        // another Session's work: a busy Workspace refuses at once.
        let request = workspace_operation::WorkspaceRequest {
            doing: format!("the change of Session {id}'s environment"),
            refused: format!("The environment of Session {id} was not changed"),
        };
        let workspace = self
            .take_session_lifecycle_workspace(id, &request.refused, false)
            .await?;
        let (mut cleanup, _named) = self
            .prepare_lifecycle_cleanup(id, &request, workspace)
            .await?;
        let _operation = match cleanup.take_operation() {
            Some(operation) => (Some(operation), None),
            None => (
                None,
                Some(
                    self.take_session_lifecycle_operation(
                        id,
                        workspace_operation::WorkspaceRequest {
                            doing: request.doing.clone(),
                            refused: request.refused.clone(),
                        },
                    )
                    .await?,
                ),
            ),
        };
        let _start = self
            .lock_session_start_for_lifecycle(id, &request.refused)
            .await?;
        self.require_runtime_admission()?;
        self.require_no_unresolved_attempt(id).await?;
        self.session_dispatch_lifecycles
            .require_native_environment_change_ready(id)?;
        let current = self
            .get_session(id)
            .await
            .ok_or_else(|| DaemonError::Session(format!("session '{id}' not found")))?;
        if current.status == axocoatl_session::SessionStatus::Closed {
            return Err(DaemonError::SessionConflict(
                "The Session closed while its runtime change was waiting; reopen it explicitly"
                    .into(),
            ));
        }
        let _environment_change = self
            .stream_bus
            .begin_session_environment_change(id, current.environment.generation);
        self.stop_session_actors_checked(id).await?;
        self.remove_session_sandbox_with_dependencies_checked(id, true)
            .await?;
        let configured = self
            .session_store
            .lock()
            .await
            .configure_environment(id, image, setup_command, setup_approved, setup_reviewed)
            .map_err(|error| DaemonError::Session(error.to_string()))?;
        self.close_session_network(id).await;
        self.session_dispatch_lifecycles
            .complete_session_cleanup(&cleanup)?;
        drop(cleanup);
        self.restore_native_lifecycle_history(&configured, false)?;
        let configured = self.resolve_operator_setup_approval(configured).await?;
        if configured.environment.state == SessionEnvironmentState::AwaitingApproval {
            Ok(configured)
        } else {
            self.prepare_session_environment(&configured).await
        }
    }
}

impl AxocoatlDaemon {
    /// Versioned endpoint keeps raw history and changes only the explicitly
    /// selected conversation baseline. Repository changes are not undone.
    pub async fn rewind_versioned_session_to_turn(
        &self,
        session_id: &str,
        keep_through: Option<&str>,
    ) -> Result<Vec<axocoatl_session::session_history::SessionHistoryEntry>, DaemonError> {
        use axocoatl_session::session_history::{HistoryVisibility, SessionHistoryEntry};
        if !self.uses_native_session_history() {
            return self
                .rewind_session_to_turn(session_id, keep_through)
                .await
                .map(|turns| {
                    turns
                        .into_iter()
                        .map(|turn| SessionHistoryEntry::LegacyV1(Box::new(turn)))
                        .collect()
                });
        }
        self.require_runtime_admission()?;
        let operation = self.attempt_operation(session_id).await;
        let _operation = operation.try_lock().map_err(|_| {
            DaemonError::SessionConflict(
                "Wait for the current Session operation to settle before rewinding".into(),
            )
        })?;
        self.require_no_unresolved_attempt(session_id).await?;
        let session = self
            .get_session(session_id)
            .await
            .ok_or_else(|| DaemonError::Session("Session not found".into()))?;
        if session.status == axocoatl_session::SessionStatus::Closed {
            return Err(DaemonError::SessionConflict(
                "Reopen this Session before rewinding".into(),
            ));
        }
        let SessionMode::SingleAgent { agent_id } = &session.mode else {
            return Err(DaemonError::SessionConflict(
                "Rewind currently requires a single autonomous Agent Session".into(),
            ));
        };
        let token = self
            .session_dispatch_lifecycles
            .session_team_token(session_id)?;
        self.session_dispatch_lifecycles.with_session_team_stores(&token,|canonical,content,memory|{
            use axocoatl_session::execution_content::ActivationEvidenceContent;
            use axocoatl_session::execution_namespace::ExecutionComponent;
            use axocoatl_session::session_team::SessionTeamStore;
            let error=|error:String|DaemonError::SessionConflict(error);
            let team=SessionTeamStore::open_owned(canonical.component_namespace(ExecutionComponent::SessionTeam).map_err(|e|error(e.to_string()))?,canonical,content,None).map_err(|e|error(e.to_string()))?;
            let conversations=if let Some(revision)=team.current().map_err(|e|error(e.to_string()))? {
                if revision.graph.slots.len()!=1 {return Err(error("Rewind currently requires a single autonomous Agent Session team".into()));}
                let slot=&revision.graph.slots[0];
                let ActivationEvidenceContent::Definition{configuration,..}=content.resolve_activation_evidence(&slot.definition.snapshot).map_err(|e|error(e.to_string()))? else {return Err(error("The exact Session Agent definition is unavailable".into()));};
                let config:axocoatl_core::AgentConfig=serde_json::from_str(&configuration).map_err(|e|error(e.to_string()))?;
                if config.role!=axocoatl_core::AgentRole::Autonomous {return Err(error("Rewind currently requires an autonomous Agent".into()));}
                vec![slot.conversation_id.clone()]
            }else {
                validate_rewind_agent(&self.config,agent_id)?;
                let conversations=memory.legacy_conversations().map_err(|e|error(e.to_string()))?;
                if conversations.len()>1 {return Err(error("This migrated Session has multiple retained Agent conversations".into()));}
                conversations
            };
            drop(team);
            memory.rewind_session(canonical,content,keep_through,&conversations,axocoatl_memory::legacy_conversation::ToolReplayPolicy::CompleteNativeGroups).map_err(|e|error(e.to_string()))?;
            let mut history=axocoatl_session::session_history::SessionHistory::from_upgraded(canonical,content).map_err(|e|error(e.to_string()))?;
            history.apply_superseded(&memory.superseded_turn_ids().map_err(|e|error(e.to_string()))?).map_err(|e|error(e.to_string()))?;
            Ok(history.entries(HistoryVisibility::Visible).into_iter().cloned().collect())
        })
    }
}

#[cfg(all(test, unix))]
#[path = "bootstrap_session_lifecycle_tests.rs"]
mod lifecycle_tests;
