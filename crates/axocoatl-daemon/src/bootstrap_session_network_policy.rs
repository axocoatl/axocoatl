//! Live network policy: the allowlists `axocoatl network reload` changes,
//! the Session decision points they apply to, and a person's decisions on
//! Agents' proposals.
//!
//! The daemon keeps the allowlists in force behind one lock. A Session's
//! decision point opens with them; a reload validates the configuration
//! file the daemon was started with, replaces them and applies them to every
//! running decision point, the Sessions' and the browser's own, then reports
//! what needs a restart instead.

use super::*;
use crate::session_egress::{EgressPolicyConfig, SessionEgress};
use crate::session_network_proposals::{NetworkProposalDecided, NetworkProposalDecisionRequest};
use crate::session_network_reload::{
    applicable_policy, live_changes, restart_required, NetworkReloadReport, ReloadFailure,
    ScopeRevision,
};
use axocoatl_session::network_record::{is_proposal_id, ProposalState};

/// The allowlists in force and the configuration the daemon started with.
pub(crate) struct LiveNetworkPolicy {
    /// Every setting other than the four allowlists applies from this until
    /// the daemon restarts.
    started: AxocoatlConfig,
    current: StdMutex<EgressPolicyConfig>,
    /// One reload at a time.
    reloads: tokio::sync::Mutex<()>,
}

impl LiveNetworkPolicy {
    pub(crate) fn new(config: &AxocoatlConfig) -> Self {
        Self {
            started: config.clone(),
            current: StdMutex::new(EgressPolicyConfig::from_config(config)),
            reloads: tokio::sync::Mutex::new(()),
        }
    }

    /// The allowlists a decision point opened now compiles from.
    pub(crate) fn current(&self) -> EgressPolicyConfig {
        self.current
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
    }

    fn replace(&self, policy: EgressPolicyConfig) {
        *self
            .current
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = policy;
    }
}

/// The daemon's Session decision points under `network: egress`, opened on
/// first use. The daemon and the browser tools share it.
pub(crate) struct SessionEgressPoints {
    lifecycles: Arc<session_dispatch::SessionDispatchRegistry>,
    points: Arc<tokio::sync::Mutex<HashMap<String, Arc<SessionEgress>>>>,
    data_root: SecureDir,
    policy: Arc<LiveNetworkPolicy>,
    records: Arc<crate::session_network::SessionNetworkRecords>,
    sandboxes: Arc<tokio::sync::Mutex<HashMap<String, Arc<dyn Sandbox>>>>,
}

impl SessionEgressPoints {
    pub(crate) fn new(
        lifecycles: Arc<session_dispatch::SessionDispatchRegistry>,
        points: Arc<tokio::sync::Mutex<HashMap<String, Arc<SessionEgress>>>>,
        data_root: SecureDir,
        policy: Arc<LiveNetworkPolicy>,
        records: Arc<crate::session_network::SessionNetworkRecords>,
        sandboxes: Arc<tokio::sync::Mutex<HashMap<String, Arc<dyn Sandbox>>>>,
    ) -> Self {
        Self {
            lifecycles,
            points,
            data_root,
            policy,
            records,
            sandboxes,
        }
    }

    /// The Session's decision point, opened on first use. Opening it
    /// replays this Session's allows, revokes and proposals from its network
    /// record and records each scope's policy when it changed. Only a native
    /// Session has the record that every decision is written to.
    pub(crate) async fn get_or_open(
        &self,
        session_id: &str,
    ) -> Result<Arc<SessionEgress>, DaemonError> {
        let native = self
            .lifecycles
            .retains_session(session_id)
            .map_err(|error| DaemonError::Session(error.to_string()))?;
        if !native {
            return Err(DaemonError::Session(
                EGRESS_NEEDS_NATIVE_SESSION.to_string(),
            ));
        }
        let mut decision_points = self.points.lock().await;
        if let Some(existing) = decision_points.get(session_id) {
            return Ok(existing.clone());
        }
        let env_dir = self.data_root.child(EGRESS_ENV_DIR).map_err(|error| {
            DaemonError::Session(format!("preparing egress credential storage: {error}"))
        })?;
        // Read under the decision points' lock, which a reload holds while it
        // replaces the policy: a decision point opened now either compiles
        // the new lists or is in the reload's list.
        let egress = SessionEgress::open(
            session_id,
            self.policy.current(),
            Arc::new(crate::session_egress::SessionRecordSink::new(
                self.records.clone(),
                session_id,
            )),
            Arc::new(crate::session_egress::SystemResolver),
            Some(env_dir),
        )
        .await
        .map_err(|error| {
            DaemonError::Session(format!("opening the Session's egress policy: {error}"))
        })?;
        decision_points.insert(session_id.to_string(), egress.clone());
        Ok(egress)
    }

    /// Replace the policy and list the decision points to apply it to, in
    /// one step under the decision points' lock.
    async fn replace_and_list(
        &self,
        policy: EgressPolicyConfig,
    ) -> Vec<(String, Arc<SessionEgress>)> {
        let points = self.points.lock().await;
        self.policy.replace(policy);
        let mut listed: Vec<(String, Arc<SessionEgress>)> = points
            .iter()
            .map(|(id, egress)| (id.clone(), egress.clone()))
            .collect();
        listed.sort_by(|left, right| left.0.cmp(&right.0));
        listed
    }
}

#[async_trait::async_trait]
impl crate::session_dispatch_browser::SessionEgressSource for SessionEgressPoints {
    async fn session_egress(&self, session_id: &str) -> Result<Arc<SessionEgress>, String> {
        self.get_or_open(session_id)
            .await
            .map_err(|error| error.to_string())
    }

    async fn session_sidecar_ready(&self, session_id: &str) -> bool {
        self.sandboxes
            .lock()
            .await
            .get(session_id)
            .and_then(|sandbox| sandbox.egress_status())
            .is_some_and(|status| {
                status.phase == axocoatl_isolation::egress_sidecar::SidecarPhase::Ready
            })
    }
}

fn proposal_target(proposal_id: &str) -> Result<(), DaemonError> {
    if is_proposal_id(proposal_id) {
        Ok(())
    } else {
        Err(DaemonError::InvalidRequest(
            "a proposal id is prop_ and 16 lowercase hex digits".into(),
        ))
    }
}

impl AxocoatlDaemon {
    /// Read the configuration file this daemon was started with again,
    /// validate all of it, and apply its allowlists (`sandbox.egress.allow`,
    /// `sandbox.egress.private_destinations`, `browser.allow`,
    /// `browser.private_destinations`) to new Sessions and to every running
    /// decision point. Other differences are reported, not applied. An
    /// invalid file changes nothing.
    pub async fn reload_network_policy(&self) -> Result<NetworkReloadReport, DaemonError> {
        let path = self
            .config_path
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
            .ok_or_else(|| {
                DaemonError::InvalidRequest(
                    "this daemon was not started from a configuration file, so there is nothing to reload"
                        .into(),
                )
            })?;
        let _serial = self.network_policy.reloads.lock().await;
        let next = axocoatl_config::load_config(&path).await.map_err(|error| {
            DaemonError::InvalidRequest(format!(
                "{} is not valid; nothing was changed:\n{error}",
                path.display()
            ))
        })?;
        let started = &self.network_policy.started;
        let current = self.network_policy.current();
        let policy = applicable_policy(started, &current, &next);
        let (applied, unchanged) = live_changes(&current, &policy);
        let mut report = NetworkReloadReport {
            applied,
            unchanged,
            restart_required: restart_required(started, &next),
            revisions: Vec::new(),
            failed: Vec::new(),
        };
        if report.applied.is_empty() {
            return Ok(report);
        }
        for (session_id, egress) in self.egress_points.replace_and_list(policy.clone()).await {
            match egress
                .reload_config(policy.clone(), SESSION_NETWORK_ACTOR)
                .await
            {
                Ok(changed) => {
                    report
                        .revisions
                        .extend(changed.into_iter().map(|scope| ScopeRevision {
                            session_id: session_id.clone(),
                            scope: scope.scope.as_str().to_string(),
                            revision: scope.revision,
                            digest: scope.digest,
                            closed: scope.closed,
                        }))
                }
                Err(error) => report.failed.push(ReloadFailure {
                    session_id,
                    error: error.to_string(),
                }),
            }
        }
        // Under `bridge` and `none` the browser has decision points of its
        // own; under `egress` its policy is a scope of each Session's,
        // reloaded above, and the service only keeps the new lists.
        if let (Some(browser), Some((allow, private))) = (&self.browser_service, &policy.browser) {
            let (revisions, failed) = browser
                .reload_declared(allow.clone(), private.clone(), SESSION_NETWORK_ACTOR)
                .await;
            report.revisions.extend(revisions);
            report.failed.extend(failed);
        }
        tracing::info!(
            applied = ?report.applied,
            restart_required = ?report.restart_required,
            changed = report.revisions.len(),
            failed = report.failed.len(),
            "reloaded the network allowlists"
        );
        Ok(report)
    }

    async fn session_network_proposal_target(
        &self,
        session_id: &str,
        proposal_id: &str,
    ) -> Result<Arc<SessionEgress>, DaemonError> {
        proposal_target(proposal_id)?;
        let (egress, _) = self
            .session_network_policy_target(session_id, "session")
            .await?;
        if egress.proposal(proposal_id).is_none() {
            return Err(DaemonError::Session(format!(
                "proposal '{proposal_id}' not found"
            )));
        }
        Ok(egress)
    }

    /// A person approves an Agent's proposal: the host is allowed for this
    /// Session (recorded with the person as actor and the proposal's id),
    /// and the waiting tool call learns it.
    pub async fn approve_session_network_proposal(
        &self,
        session_id: &str,
        proposal_id: &str,
        request: NetworkProposalDecisionRequest,
    ) -> Result<NetworkProposalDecided, DaemonError> {
        let egress = self
            .session_network_proposal_target(session_id, proposal_id)
            .await?;
        let (revision, digest) = egress
            .approve_proposal(proposal_id, SESSION_NETWORK_ACTOR, &request.command_id)
            .await
            .map_err(egress_policy_error)?;
        Ok(NetworkProposalDecided {
            proposal_id: proposal_id.to_string(),
            state: ProposalState::Approved,
            revision: Some(revision),
            digest: Some(digest),
        })
    }

    /// A person rejects an Agent's proposal. The policy does not change.
    pub async fn reject_session_network_proposal(
        &self,
        session_id: &str,
        proposal_id: &str,
        request: NetworkProposalDecisionRequest,
    ) -> Result<NetworkProposalDecided, DaemonError> {
        let egress = self
            .session_network_proposal_target(session_id, proposal_id)
            .await?;
        egress
            .reject_proposal(proposal_id, SESSION_NETWORK_ACTOR, &request.command_id)
            .await
            .map_err(egress_policy_error)?;
        Ok(NetworkProposalDecided {
            proposal_id: proposal_id.to_string(),
            state: ProposalState::Rejected,
            revision: None,
            digest: None,
        })
    }
}
