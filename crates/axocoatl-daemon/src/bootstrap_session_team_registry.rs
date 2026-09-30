//! Exact registry owner tokens for synchronous future Session configuration.
//! The token is not authority to dispatch or to resurrect a closed entry.
use super::*;
use axocoatl_memory::activation_state::ActivationStateStore;
use axocoatl_session::execution_content::ExecutionContentStore;
use axocoatl_session::execution_store::SessionExecutionStore;

#[derive(Clone)]
pub(crate) struct SessionTeamToken {
    session_id: String,
    entry: TeamEntry,
}
#[derive(Clone)]
enum TeamEntry {
    Pending(Arc<pending::PendingSessionEntry>),
    Registered(Arc<RegisteredEntry>),
}
impl SessionDispatchRegistry {
    /// Inspect retained canonical history without minting a team-edit token.
    /// Closed Sessions retain their workspace memory; this callback cannot
    /// mutate the journal, reopen execution, or acquire a repository writer.
    pub(crate) fn with_session_knowledge_history<T>(
        &self,
        session_id: &str,
        inspect: impl FnOnce(&SessionExecutionStore, &ExecutionContentStore) -> Result<T>,
    ) -> Result<T> {
        let state = self
            .state
            .lock()
            .map_err(|_| failure("Session registry failed"))?;
        if state.closed {
            return Err(failure("Daemon is shutting down"));
        }
        let read = |canonical: &SessionExecutionStore,
                    content: &mut ExecutionContentStore,
                    _: &mut ActivationStateStore| {
            if canonical.owner().session_id.as_str() != session_id {
                return Err(failure("Knowledge history belongs to another Session"));
            }
            inspect(canonical, content)
        };
        if let Some(entry) = state.pending.get(session_id) {
            if entry.retired.load(Ordering::SeqCst) {
                return Err(failure("Session history owner retired"));
            }
            entry.with_team_stores(read)
        } else if let Some(entry) = state.entries.get(session_id) {
            if entry.retired.load(Ordering::SeqCst) {
                return Err(failure("Session history owner retired"));
            }
            entry.controller.with_team_stores(read)
        } else {
            Err(failure("Session canonical history is not retained"))
        }
    }

    /// Authority review must use the current controller's exclusive writer when
    /// one exists. Pending and older turns retain the existing read-only path.
    pub(crate) fn with_session_team_grant_stores<T>(
        &self,
        token: &SessionTeamToken,
        use_stores: impl FnOnce(
            &SessionExecutionStore,
            &mut ExecutionContentStore,
            Option<(
                &LogicalTurnId,
                &axocoatl_session::control_authority::ControlAuthority,
            )>,
        ) -> Result<T>,
    ) -> Result<T> {
        match &token.entry {
            TeamEntry::Pending(_) => self
                .with_session_team_stores(token, |canonical, content, _| {
                    use_stores(canonical, content, None)
                }),
            TeamEntry::Registered(_) => self.with_session_team_controller(token, |controller| {
                controller.with_grant_stores(use_stores)
            }),
        }
    }
    /// Settlement reads the retained current writer under its controller lock.
    /// Reopening that same component would contend with our own exclusive lease.
    pub(crate) fn with_session_team_settlement_stores<T>(
        &self,
        token: &SessionTeamToken,
        use_stores: impl FnOnce(
            &SessionExecutionStore,
            Option<(
                &LogicalTurnId,
                &axocoatl_session::control_authority::ControlAuthority,
            )>,
        ) -> Result<T>,
    ) -> Result<T> {
        match &token.entry {
            TeamEntry::Pending(_) => {
                self.with_session_team_stores(token, |canonical, _, _| use_stores(canonical, None))
            }
            TeamEntry::Registered(_) => self.with_session_team_controller(token, |controller| {
                controller.with_team_work_authority(|canonical, turn_id, authority| {
                    use_stores(canonical, Some((turn_id, authority)))
                })
            }),
        }
    }
    pub(crate) fn repeated_session_human_action(
        &self,
        token: &SessionTeamToken,
        request: &crate::session_dispatch::HumanControlActionRequest,
    ) -> Result<Option<axocoatl_session::control_command::CommandReceiptView>> {
        match &token.entry {
            TeamEntry::Pending(_) => {
                self.with_session_team_stores(token, |canonical, content, _| {
                    crate::session_dispatch::human_context::existing_human_receipt(
                        canonical, content, request,
                    )
                    .map_err(|error| failure(error.to_string()))
                })
            }
            TeamEntry::Registered(_) => self.with_session_team_controller(token, |controller| {
                controller
                    .repeated_human_action(request)
                    .map_err(|error| failure(error.to_string()))
            }),
        }
    }
    pub(crate) fn repeated_session_graph_edit(
        &self,
        token: &SessionTeamToken,
        request: &super::super::session_graph::HumanGraphEditRequest,
        review: Option<&str>,
    ) -> Result<Option<super::super::session_graph::HumanGraphEditPreview>> {
        match &token.entry {
            TeamEntry::Pending(_) => {
                self.with_session_team_stores(token, |canonical, content, _| {
                    crate::session_dispatch::pending_human_graph_receipt(
                        canonical, content, request, review,
                    )
                    .map_err(|error| failure(error.to_string()))
                })
            }
            TeamEntry::Registered(_) => self.with_session_team_controller(token, |controller| {
                controller
                    .repeated_human_graph_edit(request, review)
                    .map_err(|error| failure(error.to_string()))
            }),
        }
    }
    pub(crate) fn with_session_team_controller<T>(
        &self,
        token: &SessionTeamToken,
        use_controller: impl FnOnce(&SessionDispatchController) -> Result<T>,
    ) -> Result<T> {
        let state = self
            .state
            .lock()
            .map_err(|_| failure("Session registry failed"))?;
        if state.closed || state.closing_sessions.contains(&token.session_id) {
            return Err(failure("Session is closing"));
        }
        let TeamEntry::Registered(entry) = &token.entry else {
            return Err(failure("This Session has no current command controller"));
        };
        if entry.retired.load(Ordering::SeqCst)
            || !state
                .entries
                .get(&token.session_id)
                .is_some_and(|actual| Arc::ptr_eq(entry, actual))
        {
            return Err(failure("Session graph token lost its exact controller"));
        }
        use_controller(&entry.controller)
    }
    pub(crate) fn session_team_token(&self, session_id: &str) -> Result<SessionTeamToken> {
        let state = self
            .state
            .lock()
            .map_err(|_| failure("Session registry failed"))?;
        if state.closed || state.closing_sessions.contains(session_id) {
            return Err(failure("Session is closing"));
        }
        let entry = if let Some(entry) = state.pending.get(session_id) {
            if entry.retired.load(Ordering::SeqCst) {
                return Err(failure("Session ownership retired"));
            }
            TeamEntry::Pending(entry.clone())
        } else if let Some(entry) = state.entries.get(session_id) {
            if entry.retired.load(Ordering::SeqCst) {
                return Err(failure("Session ownership retired"));
            }
            TeamEntry::Registered(entry.clone())
        } else {
            return Err(failure(
                "Session team editing requires the actual retained Session owner",
            ));
        };
        Ok(SessionTeamToken {
            session_id: session_id.into(),
            entry,
        })
    }
    /// Callback never awaits. Keep registry identity and the actual store owner
    /// locked together so Close/reopen, first Begin and Apply cannot retarget it.
    pub(crate) fn with_session_team_stores<T>(
        &self,
        token: &SessionTeamToken,
        use_stores: impl FnOnce(
            &SessionExecutionStore,
            &mut ExecutionContentStore,
            &mut ActivationStateStore,
        ) -> Result<T>,
    ) -> Result<T> {
        let state = self
            .state
            .lock()
            .map_err(|_| failure("Session registry failed"))?;
        if state.closed || state.closing_sessions.contains(&token.session_id) {
            return Err(failure("Session is closing"));
        }
        match &token.entry {
            TeamEntry::Pending(entry) => {
                if entry.retired.load(Ordering::SeqCst)
                    || !state
                        .pending
                        .get(&token.session_id)
                        .is_some_and(|actual| Arc::ptr_eq(entry, actual))
                {
                    return Err(failure("Session team token lost its exact pending owner"));
                }
                entry.with_team_stores(use_stores)
            }
            TeamEntry::Registered(entry) => {
                if entry.retired.load(Ordering::SeqCst)
                    || !state
                        .entries
                        .get(&token.session_id)
                        .is_some_and(|actual| Arc::ptr_eq(entry, actual))
                {
                    return Err(failure(
                        "Session team token lost its exact controller owner",
                    ));
                }
                entry.controller.with_team_stores(use_stores)
            }
        }
    }
}
