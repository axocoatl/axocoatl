//! `request_network_access`: an Agent asks a person for a host.
//!
//! Under `network: egress` a writer Agent that lists the tool can ask for
//! one exact host it was refused, with a reason. The request is recorded as
//! a pending proposal in the Session's network record and shown in the
//! Network panel; the call then waits (`wait_secs`, default 120, at most
//! 600) for a person to approve or reject it. Approval allows the host for
//! this Session, exactly as the panel's own allow does. If nobody decides in
//! time the call returns `pending`, and the proposal stays in the panel; a
//! later call for the same host and ports waits on it again.
//!
//! There is no way for an Agent to approve anything, and nothing approves a
//! proposal by itself. The tool is withheld outside `network: egress`, and a
//! read-only Agent (`writes: []`) or activation cannot use it.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axocoatl_session::control_authority::ExecutionProfile;
use axocoatl_session::network_record::ProposalState;
use axocoatl_tools::{BuiltinTool, ToolError};

use crate::session_dispatch::{HostInvocationContext, HostInvocationTool, HostToolDefinition};
use crate::session_dispatch_browser::SessionEgressSource;
use crate::session_egress_policy::validate_session_host;
use crate::session_network_proposals::{
    proposal_ports, proposal_reason, ProposalRequest, DEFAULT_PROPOSAL_WAIT_SECS,
    MAX_PROPOSAL_WAIT_SECS,
};

/// The tool's name.
pub(crate) const REQUEST_NETWORK_ACCESS_TOOL: &str = "request_network_access";

/// Why a read-only Agent or activation cannot ask for hosts.
pub(crate) const READ_ONLY_REFUSAL: &str = "request_network_access is for Agents that may change \
     files: read-only helpers and required checks have no network under network: egress, so a \
     host allowed for them would not be used";

/// Why the tool is not offered outside `network: egress`.
pub(crate) const NOT_EGRESS_REFUSAL: &str = "request_network_access works only under \
     sandbox.network: egress; under bridge every host is reachable and under none no host is";

const DESCRIPTION: &str = "Ask the person running this Session to allow one exact host that \
     the egress proxy refused (403 not_allowed). Give the host, the ports (default [443]) and \
     a short reason saying what the host is for. The request waits in the Session's Network \
     panel until the person approves or rejects it; this call waits up to wait_secs (default \
     120, at most 600) and returns the decision: approved (retry the connection now), \
     rejected (do not ask again; work without the host), or pending (nobody has decided yet; \
     call again with the same host and ports to keep waiting). Only a person can approve. Do \
     not ask for hosts you do not need, and never for an IP address or a wildcard.";

fn schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "host": {
                "type": "string",
                "description": "One exact host name, such as api.example.com. No wildcard and no IP address."
            },
            "ports": {
                "type": "array",
                "items": {"type": "integer", "minimum": 1, "maximum": 65535},
                "description": "Ports to allow. Defaults to [443]."
            },
            "reason": {
                "type": "string",
                "description": "What the host is for, in one or two sentences (at most 1024 bytes)."
            },
            "wait_secs": {
                "type": "integer",
                "minimum": 0,
                "maximum": MAX_PROPOSAL_WAIT_SECS,
                "description": "How long to wait for the person's decision. Defaults to 120."
            }
        },
        "required": ["host", "reason"],
        "additionalProperties": false
    })
}

/// `request_network_access` for one daemon.
pub(crate) struct RequestNetworkAccessTool {
    /// The daemon runs Sessions under `network: egress`.
    egress: bool,
    source: Arc<dyn SessionEgressSource>,
}

impl RequestNetworkAccessTool {
    pub(crate) fn new(egress: bool, source: Arc<dyn SessionEgressSource>) -> Self {
        Self { egress, source }
    }
}

fn read_only(profile: &ExecutionProfile) -> bool {
    profile.write_scope.as_ref().is_some_and(Vec::is_empty)
}

impl HostInvocationTool for RequestNetworkAccessTool {
    fn name(&self) -> &'static str {
        REQUEST_NETWORK_ACCESS_TOOL
    }

    fn definition(&self) -> Arc<dyn BuiltinTool> {
        Arc::new(HostToolDefinition::new(
            REQUEST_NETWORK_ACCESS_TOOL,
            DESCRIPTION,
            schema(),
            axocoatl_llm::ConcurrencyPolicy::Safe,
        ))
    }

    fn refusal(&self, profile: &ExecutionProfile) -> Option<String> {
        if !self.egress {
            return Some(NOT_EGRESS_REFUSAL.to_string());
        }
        read_only(profile).then(|| READ_ONLY_REFUSAL.to_string())
    }

    /// Outside `network: egress` an Agent that lists the tool runs without it.
    fn withheld(&self) -> bool {
        !self.egress
    }

    fn bind(&self, context: HostInvocationContext) -> Arc<dyn BuiltinTool> {
        Arc::new(BoundRequestNetworkAccess {
            egress: self.egress,
            source: self.source.clone(),
            context,
        })
    }
}

struct BoundRequestNetworkAccess {
    egress: bool,
    source: Arc<dyn SessionEgressSource>,
    context: HostInvocationContext,
}

impl BoundRequestNetworkAccess {
    fn failed(&self, reason: impl Into<String>) -> ToolError {
        ToolError::ExecutionFailed {
            tool: REQUEST_NETWORK_ACCESS_TOOL.into(),
            reason: reason.into(),
        }
    }
}

/// The validated arguments of one call.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct NetworkAccessArguments {
    pub host: String,
    pub ports: Vec<u16>,
    pub reason: String,
    pub wait: Duration,
}

/// Check one call's arguments like a person's per-Session allow: one exact
/// host, no wildcard, no IP address; ports 1-65535.
pub(crate) fn parse_arguments(
    arguments: &serde_json::Value,
) -> Result<NetworkAccessArguments, String> {
    let object = arguments
        .as_object()
        .ok_or("arguments must be an object with host and reason")?;
    if let Some(unknown) = object
        .keys()
        .find(|key| !matches!(key.as_str(), "host" | "ports" | "reason" | "wait_secs"))
    {
        return Err(format!("unknown argument {unknown}"));
    }
    let host = object
        .get("host")
        .and_then(serde_json::Value::as_str)
        .ok_or("host is required: one exact host name")?;
    let host = validate_session_host(host)?;
    let ports = match object.get("ports") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::Array(values)) => Some(
            values
                .iter()
                .map(|value| {
                    value
                        .as_u64()
                        .and_then(|port| u16::try_from(port).ok())
                        .ok_or_else(|| "ports must be integers 1-65535".to_string())
                })
                .collect::<Result<Vec<u16>, String>>()?,
        ),
        Some(_) => return Err("ports must be a list of integers".into()),
    };
    let ports = proposal_ports(ports.as_deref())?;
    let reason = object
        .get("reason")
        .and_then(serde_json::Value::as_str)
        .ok_or("reason is required: say what the host is for")?;
    let reason = proposal_reason(reason)?;
    let wait = match object.get("wait_secs") {
        None | Some(serde_json::Value::Null) => DEFAULT_PROPOSAL_WAIT_SECS,
        Some(value) => value
            .as_u64()
            .filter(|secs| *secs <= MAX_PROPOSAL_WAIT_SECS)
            .ok_or_else(|| format!("wait_secs must be 0-{MAX_PROPOSAL_WAIT_SECS}"))?,
    };
    Ok(NetworkAccessArguments {
        host,
        ports,
        reason,
        wait: Duration::from_secs(wait),
    })
}

#[async_trait]
impl BuiltinTool for BoundRequestNetworkAccess {
    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn parameters_schema(&self) -> serde_json::Value {
        schema()
    }

    async fn execute(&self, arguments: serde_json::Value) -> Result<serde_json::Value, ToolError> {
        if !self.egress {
            return Err(self.failed(NOT_EGRESS_REFUSAL));
        }
        if self.context.read_only {
            return Err(self.failed(READ_ONLY_REFUSAL));
        }
        let arguments = parse_arguments(&arguments).map_err(|reason| self.failed(reason))?;
        let egress = self
            .source
            .session_egress(&self.context.session_id)
            .await
            .map_err(|reason| self.failed(reason))?;
        let mut proposed = egress
            .propose(ProposalRequest {
                host: arguments.host.clone(),
                ports: arguments.ports.clone(),
                reason: arguments.reason,
                agent: self.context.agent.clone(),
                invocation_id: self.context.invocation_id.as_str().to_string(),
                activation_id: self.context.activation.activation_id.as_str().to_string(),
            })
            .await
            .map_err(|error| self.failed(error.to_string()))?;
        let waited = tokio::time::timeout(
            arguments.wait,
            proposed
                .outcome
                .wait_for(|state| *state != ProposalState::Pending),
        )
        .await;
        let decision = match waited {
            Ok(Ok(state)) => *state,
            // The decision point went away with the Session.
            Ok(Err(_)) | Err(_) => ProposalState::Pending,
        };
        let proposal = egress
            .proposal(&proposed.view.id)
            .unwrap_or_else(|| proposed.view.clone());
        let ports = proposal
            .ports
            .iter()
            .map(u16::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let message = match decision {
            ProposalState::Approved => format!(
                "A person allowed {}:{ports} for this Session. Retry the connection now.",
                proposal.host
            ),
            ProposalState::Rejected => format!(
                "A person rejected {}:{ports}. Do not ask for it again; work without it.",
                proposal.host
            ),
            ProposalState::Pending => format!(
                "Nobody has decided yet. The request stays in the Session's Network panel; call request_network_access again with host {} and the same ports to keep waiting.",
                proposal.host
            ),
        };
        let mut result = serde_json::json!({
            "decision": decision.as_str(),
            "proposal_id": proposal.id,
            "host": proposal.host,
            "ports": proposal.ports,
            "message": message,
        });
        if !proposed.created {
            result["joined"] = serde_json::Value::Bool(true);
        }
        if let (ProposalState::Approved, Some(revision)) = (decision, proposal.revision) {
            result["revision"] = serde_json::json!(revision);
        }
        Ok(result)
    }
}
