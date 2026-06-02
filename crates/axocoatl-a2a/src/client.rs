use crate::error::A2AError;
use crate::types::{A2ATask, A2ATaskResult, AgentCard};

/// A2A protocol client for discovering and delegating tasks to remote agents.
pub struct A2AClient {
    http: reqwest::Client,
}

impl A2AClient {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::new(),
        }
    }

    /// Discover a remote agent by fetching its Agent Card.
    /// Per A2A spec: GET `{endpoint}/.well-known/agent.json`
    pub async fn discover(&self, endpoint: &str) -> Result<AgentCard, A2AError> {
        let url = format!("{}/.well-known/agent.json", endpoint.trim_end_matches('/'));
        let card: AgentCard = self
            .http
            .get(&url)
            .send()
            .await?
            .json()
            .await
            .map_err(|e| A2AError::DiscoveryFailed(e.to_string()))?;
        Ok(card)
    }

    /// Send a task to a remote agent and wait for the result (non-streaming).
    pub async fn send_task(
        &self,
        receiver: &AgentCard,
        task: A2ATask,
    ) -> Result<A2ATaskResult, A2AError> {
        let url = format!("{}/tasks", receiver.endpoint.trim_end_matches('/'));
        let result: A2ATaskResult = self
            .http
            .post(&url)
            .json(&task)
            .send()
            .await?
            .json()
            .await
            .map_err(|e| A2AError::TaskFailed(e.to_string()))?;
        Ok(result)
    }
}

impl Default for A2AClient {
    fn default() -> Self {
        Self::new()
    }
}
