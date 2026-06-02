use ractor::{Actor, ActorProcessingErr, ActorRef, SupervisionEvent};
use tokio::sync::oneshot;

use axocoatl_core::{AgentConfig, AgentInput, AgentOutput, AgentStatus, TokenUsageStats};

use crate::behavior::AgentBehavior;

/// Messages that can be sent to an agent actor.
pub enum AgentMessage {
    /// Execute a task.
    Execute {
        input: AgentInput,
        reply: oneshot::Sender<Result<AgentOutput, String>>,
        /// Optional sink — when present, the agent's output is streamed to it
        /// chunk-by-chunk as the LLM generates.
        sink: Option<crate::behavior::StreamSink>,
    },
    /// Query current status.
    GetStatus(oneshot::Sender<AgentStatus>),
    /// Get cumulative token usage.
    GetTokenUsage(oneshot::Sender<TokenUsageStats>),
    /// Spawn a worker agent (coordinator mode).
    SpawnWorker {
        config: AgentConfig,
        reply: oneshot::Sender<Result<(), String>>,
    },
    /// Receive a result from a worker agent.
    WorkerResult {
        worker_id: String,
        output: Result<AgentOutput, String>,
    },
}

// ractor requires Message: Send + 'static
// We can't derive Debug because oneshot::Sender doesn't impl Debug nicely,
// but ractor only needs Send + 'static.
impl std::fmt::Debug for AgentMessage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AgentMessage::Execute { .. } => write!(f, "AgentMessage::Execute"),
            AgentMessage::GetStatus(_) => write!(f, "AgentMessage::GetStatus"),
            AgentMessage::GetTokenUsage(_) => write!(f, "AgentMessage::GetTokenUsage"),
            AgentMessage::SpawnWorker { .. } => write!(f, "AgentMessage::SpawnWorker"),
            AgentMessage::WorkerResult { worker_id, .. } => {
                write!(f, "AgentMessage::WorkerResult({worker_id})")
            }
        }
    }
}

/// Persistent state held by each agent actor between messages.
pub struct AgentActorState {
    pub config: AgentConfig,
    pub status: AgentStatus,
    pub behavior: Box<dyn AgentBehavior>,
    pub token_usage: TokenUsageStats,
}

/// The ractor Actor wrapper for Axocoatl agents.
pub struct AgentActor;

impl Actor for AgentActor {
    type Msg = AgentMessage;
    type State = AgentActorState;
    type Arguments = (AgentConfig, Box<dyn AgentBehavior>);

    async fn pre_start(
        &self,
        _myself: ActorRef<Self::Msg>,
        (config, mut behavior): Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        behavior
            .on_start(&config)
            .await
            .map_err(|e| ActorProcessingErr::from(e.to_string()))?;

        tracing::info!(agent_id = %config.id, "Agent started");

        Ok(AgentActorState {
            config,
            status: AgentStatus::Idle,
            behavior,
            token_usage: TokenUsageStats::default(),
        })
    }

    async fn handle(
        &self,
        _myself: ActorRef<Self::Msg>,
        msg: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        match msg {
            AgentMessage::Execute { input, reply, sink } => {
                state.status = AgentStatus::Running;

                // Attach the streaming sink for this execution, then clear it
                // afterwards so a later non-streaming call doesn't reuse it.
                let streaming = sink.is_some();
                if streaming {
                    state.behavior.set_stream_sink(sink);
                }
                let result = state.behavior.execute(input).await;
                if streaming {
                    state.behavior.set_stream_sink(None);
                }

                match result {
                    Ok(output) => {
                        state.token_usage.merge(&output.token_usage);
                        state.status = AgentStatus::Idle;
                        tracing::debug!(agent_id = %state.config.id, "Execution complete");
                        let _ = reply.send(Ok(output));
                    }
                    Err(e) => {
                        let err_msg = e.to_string();
                        state.status = AgentStatus::Failed {
                            error: err_msg.clone(),
                            restarts: 0,
                        };
                        let _ = reply.send(Err(err_msg.clone()));
                        return Err(ActorProcessingErr::from(err_msg));
                    }
                }
            }
            AgentMessage::GetStatus(reply) => {
                let _ = reply.send(state.status.clone());
            }
            AgentMessage::GetTokenUsage(reply) => {
                let _ = reply.send(state.token_usage.clone());
            }
            AgentMessage::SpawnWorker { config, reply } => {
                tracing::info!(
                    parent = %state.config.id,
                    worker = %config.id,
                    "SpawnWorker request received"
                );
                // Coordinator handles spawning internally via CoordinatorBehavior
                let _ = reply.send(Ok(()));
            }
            AgentMessage::WorkerResult { worker_id, output } => match &output {
                Ok(o) => {
                    state.token_usage.merge(&o.token_usage);
                    tracing::debug!(worker = %worker_id, "Worker result received");
                }
                Err(e) => {
                    tracing::warn!(worker = %worker_id, error = %e, "Worker failed");
                }
            },
        }
        Ok(())
    }

    async fn handle_supervisor_evt(
        &self,
        _myself: ActorRef<Self::Msg>,
        msg: SupervisionEvent,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        match msg {
            SupervisionEvent::ActorFailed(dead_actor, err) => {
                tracing::warn!(
                    supervisor = %state.config.id,
                    failed_child = %dead_actor.get_name().unwrap_or("unknown".to_string()),
                    error = %err,
                    "Child agent failed"
                );
            }
            SupervisionEvent::ActorTerminated(actor_cell, _, _) => {
                tracing::info!(
                    actor = %actor_cell.get_name().unwrap_or("unknown".to_string()),
                    "Child actor terminated normally"
                );
            }
            _ => {}
        }
        Ok(())
    }

    async fn post_stop(
        &self,
        _myself: ActorRef<Self::Msg>,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        state
            .behavior
            .on_stop()
            .await
            .map_err(|e| ActorProcessingErr::from(e.to_string()))?;
        tracing::info!(agent_id = %state.config.id, "Agent stopped");
        Ok(())
    }
}

/// Helper: send Execute message and await the response.
pub async fn execute_agent(
    actor: &ActorRef<AgentMessage>,
    input: AgentInput,
) -> Result<AgentOutput, String> {
    let (tx, rx) = oneshot::channel();
    actor
        .cast(AgentMessage::Execute {
            input,
            reply: tx,
            sink: None,
        })
        .map_err(|e| format!("Failed to send to agent: {e}"))?;
    rx.await
        .map_err(|_| "Agent dropped reply channel".to_string())?
}

/// Helper: execute an agent while streaming its output chunks to `sink`.
/// Returns the final `AgentOutput` once generation completes.
pub async fn execute_agent_streaming(
    actor: &ActorRef<AgentMessage>,
    input: AgentInput,
    sink: crate::behavior::StreamSink,
) -> Result<AgentOutput, String> {
    let (tx, rx) = oneshot::channel();
    actor
        .cast(AgentMessage::Execute {
            input,
            reply: tx,
            sink: Some(sink),
        })
        .map_err(|e| format!("Failed to send to agent: {e}"))?;
    rx.await
        .map_err(|_| "Agent dropped reply channel".to_string())?
}

/// Helper: query agent status.
pub async fn get_agent_status(actor: &ActorRef<AgentMessage>) -> Result<AgentStatus, String> {
    let (tx, rx) = oneshot::channel();
    actor
        .cast(AgentMessage::GetStatus(tx))
        .map_err(|e| format!("Failed to send to agent: {e}"))?;
    rx.await
        .map_err(|_| "Agent dropped reply channel".to_string())
}

/// Helper: query cumulative token usage for an agent.
pub async fn get_agent_token_usage(
    actor: &ActorRef<AgentMessage>,
) -> Result<TokenUsageStats, String> {
    let (tx, rx) = oneshot::channel();
    actor
        .cast(AgentMessage::GetTokenUsage(tx))
        .map_err(|e| format!("Failed to send to agent: {e}"))?;
    rx.await
        .map_err(|_| "Agent dropped reply channel".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axocoatl_core::{AgentConfig, AgentId, AgentInput, AgentOutput, TokenUsageStats};
    /// A simple echo behavior for testing.
    struct EchoBehavior;

    #[async_trait::async_trait]
    impl AgentBehavior for EchoBehavior {
        async fn on_start(&mut self, _config: &AgentConfig) -> Result<(), crate::AgentError> {
            Ok(())
        }
        async fn execute(&mut self, input: AgentInput) -> Result<AgentOutput, crate::AgentError> {
            Ok(AgentOutput {
                content: format!("Echo: {}", input.content),
                tool_calls: vec![],
                token_usage: TokenUsageStats::new(10, 5),
            })
        }
        async fn on_stop(&mut self) -> Result<(), crate::AgentError> {
            Ok(())
        }
    }

    /// A behavior that fails on every call.
    struct FailBehavior;

    #[async_trait::async_trait]
    impl AgentBehavior for FailBehavior {
        async fn on_start(&mut self, _config: &AgentConfig) -> Result<(), crate::AgentError> {
            Ok(())
        }
        async fn execute(&mut self, _input: AgentInput) -> Result<AgentOutput, crate::AgentError> {
            Err(crate::AgentError::Internal(
                "intentional failure".to_string(),
            ))
        }
        async fn on_stop(&mut self) -> Result<(), crate::AgentError> {
            Ok(())
        }
    }

    fn test_config() -> AgentConfig {
        AgentConfig {
            id: AgentId::new("test-agent"),
            name: "Test Agent".to_string(),
            ..AgentConfig::default()
        }
    }

    #[tokio::test]
    async fn spawn_and_execute() {
        let (actor_ref, handle) = AgentActor::spawn(
            Some("test-echo".to_string()),
            AgentActor,
            (test_config(), Box::new(EchoBehavior)),
        )
        .await
        .unwrap();

        let output = execute_agent(&actor_ref, AgentInput::text("hello")).await;
        assert!(output.is_ok());
        assert_eq!(output.unwrap().content, "Echo: hello");

        actor_ref.stop(None);
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn get_status_idle() {
        let (actor_ref, handle) = AgentActor::spawn(
            Some("test-status".to_string()),
            AgentActor,
            (test_config(), Box::new(EchoBehavior)),
        )
        .await
        .unwrap();

        let status = get_agent_status(&actor_ref).await.unwrap();
        assert_eq!(status, AgentStatus::Idle);

        actor_ref.stop(None);
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn token_usage_accumulates() {
        let (actor_ref, handle) = AgentActor::spawn(
            Some("test-tokens".to_string()),
            AgentActor,
            (test_config(), Box::new(EchoBehavior)),
        )
        .await
        .unwrap();

        // Execute twice
        execute_agent(&actor_ref, AgentInput::text("first"))
            .await
            .unwrap();
        execute_agent(&actor_ref, AgentInput::text("second"))
            .await
            .unwrap();

        // Check accumulated token usage
        let (tx, rx) = oneshot::channel();
        actor_ref.cast(AgentMessage::GetTokenUsage(tx)).unwrap();
        let usage = rx.await.unwrap();
        assert_eq!(usage.input_tokens, 20); // 10 + 10
        assert_eq!(usage.output_tokens, 10); // 5 + 5

        actor_ref.stop(None);
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn failed_execution_returns_error() {
        let (actor_ref, handle) = AgentActor::spawn(
            Some("test-fail".to_string()),
            AgentActor,
            (test_config(), Box::new(FailBehavior)),
        )
        .await
        .unwrap();

        let result = execute_agent(&actor_ref, AgentInput::text("trigger failure")).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("intentional failure"));

        // Actor may have crashed due to the error — wait for handle
        let _ = handle.await;
    }

    #[tokio::test]
    async fn multiple_agents_independent() {
        let (ref1, h1) = AgentActor::spawn(
            Some("agent-1".to_string()),
            AgentActor,
            (
                AgentConfig {
                    id: AgentId::new("agent-1"),
                    ..AgentConfig::default()
                },
                Box::new(EchoBehavior),
            ),
        )
        .await
        .unwrap();

        let (ref2, h2) = AgentActor::spawn(
            Some("agent-2".to_string()),
            AgentActor,
            (
                AgentConfig {
                    id: AgentId::new("agent-2"),
                    ..AgentConfig::default()
                },
                Box::new(EchoBehavior),
            ),
        )
        .await
        .unwrap();

        let out1 = execute_agent(&ref1, AgentInput::text("from agent 1")).await;
        let out2 = execute_agent(&ref2, AgentInput::text("from agent 2")).await;

        assert_eq!(out1.unwrap().content, "Echo: from agent 1");
        assert_eq!(out2.unwrap().content, "Echo: from agent 2");

        ref1.stop(None);
        ref2.stop(None);
        h1.await.unwrap();
        h2.await.unwrap();
    }
}
