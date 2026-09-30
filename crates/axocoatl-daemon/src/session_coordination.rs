use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use axocoatl_tools::{BuiltinTool, ToolError};

pub(crate) const COORDINATION_SIGNAL_TOOL: &str = "coordination_signal";
const SIGNAL_SUMMARY_MAX_BYTES: usize = 2 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CoordinationSignal {
    pub(crate) requester: String,
    pub(crate) target_agent: String,
    pub(crate) summary: String,
    pub(crate) generation: u32,
}

#[derive(Default)]
struct RouterState {
    next_lease: u64,
    active: HashMap<(String, String), ActiveRoute>,
}

struct ActiveRoute {
    lease: u64,
    turn_id: String,
    generation: u32,
    allowed_targets: HashSet<String>,
    signals: Vec<CoordinationSignal>,
    signal_sent: bool,
}

/// Process-local authority for the feedback tool. The tool exists on a
/// long-lived Session actor, but accepts a signal only while that exact actor
/// is executing an active coordinated generation.
#[derive(Clone, Default)]
pub(crate) struct CoordinationSignalRouter {
    state: Arc<Mutex<RouterState>>,
}

impl CoordinationSignalRouter {
    pub(crate) fn activate(
        &self,
        session_id: &str,
        turn_id: &str,
        agent_id: &str,
        generation: u32,
        allowed_targets: impl IntoIterator<Item = String>,
    ) -> Result<CoordinationSignalLease, String> {
        let key = (session_id.to_string(), agent_id.to_string());
        let mut state = self
            .state
            .lock()
            .map_err(|_| "coordination signal router is unavailable".to_string())?;
        if state.active.contains_key(&key) {
            return Err(format!(
                "Agent '{agent_id}' already owns an active coordination generation"
            ));
        }
        state.next_lease = state.next_lease.saturating_add(1);
        let lease = state.next_lease;
        state.active.insert(
            key.clone(),
            ActiveRoute {
                lease,
                turn_id: turn_id.to_string(),
                generation,
                allowed_targets: allowed_targets.into_iter().collect(),
                signals: Vec::new(),
                signal_sent: false,
            },
        );
        Ok(CoordinationSignalLease {
            router: self.clone(),
            key,
            lease,
        })
    }

    fn advertisable_targets(&self, session_id: &str, requester: &str) -> Option<Vec<String>> {
        let key = (session_id.to_string(), requester.to_string());
        let state = self.state.lock().ok()?;
        let route = state.active.get(&key)?;
        if route.signal_sent || route.allowed_targets.is_empty() {
            return None;
        }
        let mut targets = route.allowed_targets.iter().cloned().collect::<Vec<_>>();
        targets.sort();
        Some(targets)
    }

    fn publish(
        &self,
        session_id: &str,
        requester: &str,
        target_agent: String,
        summary: String,
    ) -> Result<(String, u32), ToolError> {
        let key = (session_id.to_string(), requester.to_string());
        let mut state = self.state.lock().map_err(|_| ToolError::ExecutionFailed {
            tool: COORDINATION_SIGNAL_TOOL.to_string(),
            reason: "coordination signal router is unavailable".to_string(),
        })?;
        let route = state
            .active
            .get_mut(&key)
            .ok_or_else(|| ToolError::ExecutionFailed {
                tool: COORDINATION_SIGNAL_TOOL.to_string(),
                reason:
                    "this tool is active only during a coordinated multi-agent Session generation"
                        .to_string(),
            })?;
        if !route.allowed_targets.contains(&target_agent) {
            return Err(ToolError::InvalidArgs {
                tool: COORDINATION_SIGNAL_TOOL.to_string(),
                reason: format!(
                    "Agent '{target_agent}' is not an eligible upstream ancestor for this generation"
                ),
            });
        }
        if route.signal_sent {
            return Err(ToolError::ExecutionFailed {
                tool: COORDINATION_SIGNAL_TOOL.to_string(),
                reason: "this generation already requested its one allowed upstream revision"
                    .to_string(),
            });
        }
        route.signals.push(CoordinationSignal {
            requester: requester.to_string(),
            target_agent,
            summary,
            generation: route.generation,
        });
        route.signal_sent = true;
        Ok((route.turn_id.clone(), route.generation))
    }
}

pub(crate) struct CoordinationSignalLease {
    router: CoordinationSignalRouter,
    key: (String, String),
    lease: u64,
}

impl CoordinationSignalLease {
    pub(crate) fn take_signals(&self) -> Vec<CoordinationSignal> {
        let Ok(mut state) = self.router.state.lock() else {
            return Vec::new();
        };
        state
            .active
            .get_mut(&self.key)
            .filter(|route| route.lease == self.lease)
            .map(|route| std::mem::take(&mut route.signals))
            .unwrap_or_default()
    }
}

impl Drop for CoordinationSignalLease {
    fn drop(&mut self) {
        if let Ok(mut state) = self.router.state.lock() {
            if state
                .active
                .get(&self.key)
                .is_some_and(|route| route.lease == self.lease)
            {
                state.active.remove(&self.key);
            }
        }
    }
}

pub(crate) struct CoordinationSignalTool {
    router: CoordinationSignalRouter,
    session_id: String,
    agent_id: String,
}

fn coordination_signal_parameters_schema(targets: Vec<String>) -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "kind": {
                "type": "string",
                "enum": ["changes_requested"]
            },
            "target_agent": {
                "type": "string",
                "enum": targets,
                "description": "Exact eligible upstream Agent ID for this activation"
            },
            "summary": {
                "type": "string",
                "minLength": 1,
                "description": "Concrete requested change, maximum 2048 UTF-8 bytes"
            }
        },
        "required": ["kind", "target_agent", "summary"]
    })
}

impl CoordinationSignalTool {
    pub(crate) fn new(
        router: CoordinationSignalRouter,
        session_id: impl Into<String>,
        agent_id: impl Into<String>,
    ) -> Self {
        Self {
            router,
            session_id: session_id.into(),
            agent_id: agent_id.into(),
        }
    }
}

#[async_trait::async_trait]
impl BuiltinTool for CoordinationSignalTool {
    fn description(&self) -> &str {
        "Request one bounded revision from an upstream Agent, then wait to re-verify its updated work"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        coordination_signal_parameters_schema(
            self.router
                .advertisable_targets(&self.session_id, &self.agent_id)
                .unwrap_or_default(),
        )
    }

    fn advertised_parameters_schema(&self) -> Option<serde_json::Value> {
        self.router
            .advertisable_targets(&self.session_id, &self.agent_id)
            .map(coordination_signal_parameters_schema)
    }

    fn concurrency_policy(&self) -> axocoatl_llm::ConcurrencyPolicy {
        axocoatl_llm::ConcurrencyPolicy::Exclusive
    }

    async fn execute(&self, arguments: serde_json::Value) -> Result<serde_json::Value, ToolError> {
        let object = arguments
            .as_object()
            .ok_or_else(|| ToolError::InvalidArgs {
                tool: COORDINATION_SIGNAL_TOOL.to_string(),
                reason: "expected an object".to_string(),
            })?;
        if object
            .keys()
            .any(|key| !matches!(key.as_str(), "kind" | "target_agent" | "summary"))
        {
            return Err(ToolError::InvalidArgs {
                tool: COORDINATION_SIGNAL_TOOL.to_string(),
                reason: "only kind, target_agent, and summary are permitted".to_string(),
            });
        }
        if object.get("kind").and_then(serde_json::Value::as_str) != Some("changes_requested") {
            return Err(ToolError::InvalidArgs {
                tool: COORDINATION_SIGNAL_TOOL.to_string(),
                reason: "kind must be 'changes_requested'".to_string(),
            });
        }
        let target_agent = object
            .get("target_agent")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| ToolError::InvalidArgs {
                tool: COORDINATION_SIGNAL_TOOL.to_string(),
                reason: "target_agent must be a non-empty string".to_string(),
            })?;
        let summary = object
            .get("summary")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| ToolError::InvalidArgs {
                tool: COORDINATION_SIGNAL_TOOL.to_string(),
                reason: "summary must be a non-empty string".to_string(),
            })?;
        if summary.len() > SIGNAL_SUMMARY_MAX_BYTES {
            return Err(ToolError::InvalidArgs {
                tool: COORDINATION_SIGNAL_TOOL.to_string(),
                reason: format!(
                    "summary is {} bytes; the limit is {SIGNAL_SUMMARY_MAX_BYTES} bytes",
                    summary.len()
                ),
            });
        }
        let (turn_id, generation) = self.router.publish(
            &self.session_id,
            &self.agent_id,
            target_agent.to_string(),
            summary.to_string(),
        )?;
        Ok(serde_json::json!({
            "accepted": true,
            "kind": "changes_requested",
            "turn_id": turn_id,
            "requester": self.agent_id,
            "target_agent": target_agent,
            "generation": generation,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axocoatl_tools::ToolExecutor;

    #[tokio::test]
    async fn signal_is_bounded_to_the_active_generation_and_one_request() {
        let router = CoordinationSignalRouter::default();
        let tool = CoordinationSignalTool::new(router.clone(), "session", "review");
        assert!(tool
            .execute(serde_json::json!({
                "kind": "changes_requested",
                "target_agent": "build",
                "summary": "fix the failing test"
            }))
            .await
            .unwrap_err()
            .to_string()
            .contains("only during"));

        let lease = router
            .activate("session", "turn", "review", 1, ["build".to_string()])
            .unwrap();
        tool.execute(serde_json::json!({
            "kind": "changes_requested",
            "target_agent": "build",
            "summary": "fix the failing test"
        }))
        .await
        .unwrap();
        assert_eq!(lease.take_signals().len(), 1);
        assert!(tool
            .execute(serde_json::json!({
                "kind": "changes_requested",
                "target_agent": "build",
                "summary": "and update the assertion"
            }))
            .await
            .unwrap_err()
            .to_string()
            .contains("one allowed"));
        drop(lease);
        assert!(tool
            .execute(serde_json::json!({
                "kind": "changes_requested",
                "target_agent": "build",
                "summary": "late"
            }))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn advertisement_tracks_exact_unspent_activation_targets() {
        let router = CoordinationSignalRouter::default();
        let mut executor = ToolExecutor::new();
        executor.register_builtin(
            COORDINATION_SIGNAL_TOOL,
            Arc::new(CoordinationSignalTool::new(
                router.clone(),
                "session",
                "review",
            )),
        );

        assert!(executor.as_llm_tools().is_empty());
        let empty = router
            .activate("session", "turn-empty", "review", 1, Vec::<String>::new())
            .unwrap();
        assert!(executor.as_llm_tools().is_empty());
        drop(empty);

        let lease = router
            .activate(
                "session",
                "turn",
                "review",
                1,
                ["source".to_string(), "build".to_string()],
            )
            .unwrap();
        let advertised = executor.as_llm_tools();
        assert_eq!(advertised.len(), 1);
        assert_eq!(
            advertised[0].parameters["properties"]["target_agent"]["enum"],
            serde_json::json!(["build", "source"])
        );
        assert!(advertised[0].parameters["properties"]["summary"]
            .get("maxLength")
            .is_none());

        assert!(executor
            .execute(
                COORDINATION_SIGNAL_TOOL,
                serde_json::json!({
                    "kind": "changes_requested",
                    "target_agent": "invented",
                    "summary": "not eligible"
                }),
            )
            .await
            .unwrap_err()
            .to_string()
            .contains("not an eligible"));
        assert_eq!(executor.as_llm_tools().len(), 1);

        executor
            .execute(
                COORDINATION_SIGNAL_TOOL,
                serde_json::json!({
                    "kind": "changes_requested",
                    "target_agent": "build",
                    "summary": "fix the failing test"
                }),
            )
            .await
            .unwrap();
        assert!(executor.as_llm_tools().is_empty());
        assert_eq!(lease.take_signals().len(), 1);
        assert!(executor
            .execute(
                COORDINATION_SIGNAL_TOOL,
                serde_json::json!({
                    "kind": "changes_requested",
                    "target_agent": "source",
                    "summary": "second request"
                }),
            )
            .await
            .unwrap_err()
            .to_string()
            .contains("one allowed"));
        drop(lease);
        assert!(executor.as_llm_tools().is_empty());
    }

    #[tokio::test]
    async fn summary_limit_is_utf8_bytes_not_schema_characters() {
        let router = CoordinationSignalRouter::default();
        let tool = CoordinationSignalTool::new(router.clone(), "session", "review");
        let too_large = "é".repeat((SIGNAL_SUMMARY_MAX_BYTES / 2) + 1);
        let lease = router
            .activate("session", "turn", "review", 1, ["build".to_string()])
            .unwrap();
        let error = tool
            .execute(serde_json::json!({
                "kind": "changes_requested",
                "target_agent": "build",
                "summary": too_large,
            }))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("2048 bytes"));
        assert!(lease.take_signals().is_empty());

        tool.execute(serde_json::json!({
            "kind": "changes_requested",
            "target_agent": "build",
            "summary": "é".repeat(SIGNAL_SUMMARY_MAX_BYTES / 2),
        }))
        .await
        .unwrap();
        assert_eq!(lease.take_signals().len(), 1);
    }
}
