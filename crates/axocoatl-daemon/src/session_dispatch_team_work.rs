use super::*;

impl SessionDispatchController {
    pub(crate) fn with_team_work_authority<T>(
        &self,
        use_authority: impl FnOnce(
            &SessionExecutionStore,
            &LogicalTurnId,
            &axocoatl_session::control_authority::ControlAuthority,
        ) -> std::result::Result<T, crate::error::DaemonError>,
    ) -> std::result::Result<T, crate::error::DaemonError> {
        let failure = |error: SessionDispatchError| {
            crate::error::DaemonError::SessionConflict(error.to_string())
        };
        let state = self.lock().map_err(failure)?;
        state.ready().map_err(failure)?;
        use_authority(&state.canonical, &state.turn_id, &state.authority)
    }
    pub(crate) fn install_team_work_allocations(
        &self,
        allocations: &[axocoatl_session::team_work::DurableTeamWorkAllocation],
        repository: &EvidenceRef,
    ) -> Result<()> {
        self.install_admission_grants()?;
        let mut state = self.lock()?;
        state.ready()?;
        let snapshot = state.canonical.snapshot(&state.turn_id).map_err(error)?;
        let owner = state
            .repository_owners
            .get(repository)
            .ok_or_else(|| error("Standing checks have no actual repository owner"))?;
        repository::validate_retained_repository(&state, owner, repository)?;
        let backend = owner.backend().to_owned();
        for allocation in allocations {
            let result = state
                .authority
                .apply_team_work_allocation(allocation)
                .map_err(error);
            state.fail_closed(result)?;
            let result = state
                .authority
                .authorize_team_work_conditions(
                    allocation,
                    &snapshot,
                    &state.content,
                    repository,
                    &backend,
                )
                .map_err(error);
            state.fail_closed(result)?;
        }
        Ok(())
    }
}
