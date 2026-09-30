//! Existing authenticated Session surface for reviewing delegated authority.
use super::*;
use crate::session_dispatch::{
    SessionGrantChange, SessionGrantDecision, SessionGrantPreview, SessionGrantView,
};
impl AxocoatlDaemon {
    fn with_grant_controller<T>(
        &self,
        id: &str,
        turn: &str,
        f: impl FnOnce(
            &crate::session_dispatch::SessionDispatchController,
        ) -> Result<T, crate::session_dispatch::SessionDispatchError>,
    ) -> Result<T, DaemonError> {
        let token = self.session_dispatch_lifecycles.session_team_token(id)?;
        self.session_dispatch_lifecycles
            .with_session_team_controller(&token, |controller| {
                if controller
                    .snapshot()
                    .map_err(|error| DaemonError::SessionConflict(error.to_string()))?
                    .turn_id()
                    .as_str()
                    != turn
                {
                    return Err(DaemonError::SessionConflict(
                        "Grant request belongs to another turn".into(),
                    ));
                }
                f(controller).map_err(|error| DaemonError::SessionConflict(error.to_string()))
            })
    }
    pub async fn session_control_grants(
        &self,
        id: &str,
        turn: &str,
    ) -> Result<SessionGrantView, DaemonError> {
        let token = self.session_dispatch_lifecycles.session_team_token(id)?;
        self.session_dispatch_lifecycles
            .with_session_team_grant_stores(&token, |canonical, content, held| {
                crate::session_dispatch::retained_grant_view(
                    canonical,
                    content,
                    &axocoatl_session::turn_contract::LogicalTurnId::new(turn)
                        .map_err(|error| DaemonError::SessionConflict(error.to_string()))?,
                    held,
                )
                .map_err(|error| DaemonError::SessionConflict(error.to_string()))
            })
    }
    pub async fn preview_session_grant(
        &self,
        id: &str,
        turn: &str,
        request: SessionGrantChange,
    ) -> Result<SessionGrantPreview, DaemonError> {
        self.with_grant_controller(id, turn, |controller| {
            controller.preview_grant_change(&request)
        })
    }
    pub async fn decide_session_grant(
        &self,
        id: &str,
        turn: &str,
        decision: SessionGrantDecision,
    ) -> Result<SessionGrantView, DaemonError> {
        let token = self.session_dispatch_lifecycles.session_team_token(id)?;
        if let Some(view) = self
            .session_dispatch_lifecycles
            .with_session_team_grant_stores(&token, |canonical, content, held| {
                crate::session_dispatch::retained_grant_decision(
                    canonical,
                    content,
                    &axocoatl_session::turn_contract::LogicalTurnId::new(turn)
                        .map_err(|error| DaemonError::SessionConflict(error.to_string()))?,
                    &decision,
                    held,
                )
                .map_err(|error| DaemonError::SessionConflict(error.to_string()))
            })?
        {
            return Ok(view);
        }
        self.with_grant_controller(id, turn, |controller| {
            controller.decide_grant_change(&decision)
        })
    }
    pub async fn revoke_session_grant(
        &self,
        id: &str,
        turn: &str,
        grant: &str,
        revision: u64,
    ) -> Result<SessionGrantView, DaemonError> {
        self.with_grant_controller(id, turn, |controller| {
            controller.revoke_control_grant(grant, revision)
        })
    }
}
