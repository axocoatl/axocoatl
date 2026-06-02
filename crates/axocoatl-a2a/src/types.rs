use serde::{Deserialize, Serialize};

/// Agent Card — describes an agent's capabilities to other agents (A2A v0.3 spec).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentCard {
    pub id: String,
    pub name: String,
    pub description: String,
    pub version: String,
    pub endpoint: String,
    pub capabilities: Vec<String>,
    pub input_schema: serde_json::Value,
    pub output_schema: serde_json::Value,
    pub authentication: AuthSpec,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthSpec {
    pub scheme: String,
    pub endpoint: Option<String>,
}

/// A task sent from one agent to another.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct A2ATask {
    pub id: String,
    pub sender_id: String,
    pub receiver_id: String,
    pub input: serde_json::Value,
    pub context: TaskContext,
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskContext {
    pub workflow_id: Option<String>,
    pub correlation_id: String,
    pub token_budget: Option<usize>,
}

/// Task result returned by the receiving agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct A2ATaskResult {
    pub task_id: String,
    pub status: TaskStatus,
    pub output: Option<serde_json::Value>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TaskStatus {
    Pending,
    Running,
    Completed,
    Failed,
    Cancelled,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_card_serde_roundtrip() {
        let card = AgentCard {
            id: "agent-1".to_string(),
            name: "Research Agent".to_string(),
            description: "Researches topics".to_string(),
            version: "0.1.0".to_string(),
            endpoint: "https://agent.example.com".to_string(),
            capabilities: vec!["web_search".to_string(), "summarize".to_string()],
            input_schema: serde_json::json!({"type": "object"}),
            output_schema: serde_json::json!({"type": "object"}),
            authentication: AuthSpec {
                scheme: "bearer".to_string(),
                endpoint: None,
            },
        };
        let json = serde_json::to_string(&card).unwrap();
        let back: AgentCard = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, "agent-1");
        assert_eq!(back.capabilities.len(), 2);
    }

    #[test]
    fn a2a_task_serde_roundtrip() {
        let task = A2ATask {
            id: "task-1".to_string(),
            sender_id: "agent-a".to_string(),
            receiver_id: "agent-b".to_string(),
            input: serde_json::json!({"query": "quantum computing"}),
            context: TaskContext {
                workflow_id: Some("wf-1".to_string()),
                correlation_id: "corr-1".to_string(),
                token_budget: Some(5000),
            },
            timeout_secs: Some(60),
        };
        let json = serde_json::to_string(&task).unwrap();
        let back: A2ATask = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, "task-1");
        assert_eq!(back.context.token_budget, Some(5000));
    }

    #[test]
    fn task_status_serde() {
        let statuses = vec![
            TaskStatus::Pending,
            TaskStatus::Running,
            TaskStatus::Completed,
            TaskStatus::Failed,
            TaskStatus::Cancelled,
        ];
        for status in statuses {
            let json = serde_json::to_string(&status).unwrap();
            let back: TaskStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(back, status);
        }
    }
}
