use std::collections::HashMap;
use std::sync::Arc;

use ractor::ActorRef;
use tokio::sync::RwLock;

use axocoatl_core::AgentId;

use crate::actor_impl::AgentMessage;
use crate::error::RegistryError;

/// Global registry of running agent actors.
#[derive(Clone)]
pub struct AgentRegistry {
    agents: Arc<RwLock<HashMap<AgentId, ActorRef<AgentMessage>>>>,
}

impl AgentRegistry {
    pub fn new() -> Self {
        Self {
            agents: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub async fn register(&self, id: AgentId, actor: ActorRef<AgentMessage>) {
        self.agents.write().await.insert(id, actor);
    }

    pub async fn get(&self, id: &AgentId) -> Option<ActorRef<AgentMessage>> {
        self.agents.read().await.get(id).cloned()
    }

    pub async fn remove(&self, id: &AgentId) -> Option<ActorRef<AgentMessage>> {
        self.agents.write().await.remove(id)
    }

    pub async fn send(&self, id: &AgentId, msg: AgentMessage) -> Result<(), RegistryError> {
        match self.get(id).await {
            Some(actor) => actor
                .cast(msg)
                .map_err(|e| RegistryError::SendFailed(e.to_string())),
            None => Err(RegistryError::NotFound(id.to_string())),
        }
    }

    /// Whether the agent's actor is still running. Returns false if the agent
    /// is unknown or its actor has stopped (crashed or shut down) — the signal
    /// the daemon's supervision loop uses to trigger a restart-from-checkpoint.
    pub async fn is_alive(&self, id: &AgentId) -> bool {
        match self.get(id).await {
            Some(actor) => !matches!(actor.get_status(), ractor::ActorStatus::Stopped),
            None => false,
        }
    }

    pub async fn list_ids(&self) -> Vec<AgentId> {
        self.agents.read().await.keys().cloned().collect()
    }

    pub async fn count(&self) -> usize {
        self.agents.read().await.len()
    }
}

impl Default for AgentRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actor_impl::AgentActor;
    use crate::behavior::AgentBehavior;
    use axocoatl_core::{AgentConfig, AgentInput, AgentOutput};
    use ractor::Actor;

    struct NoopBehavior;
    #[async_trait::async_trait]
    impl AgentBehavior for NoopBehavior {
        async fn on_start(&mut self, _: &AgentConfig) -> Result<(), crate::AgentError> {
            Ok(())
        }
        async fn execute(&mut self, _: AgentInput) -> Result<AgentOutput, crate::AgentError> {
            Ok(AgentOutput::text("noop"))
        }
        async fn on_stop(&mut self) -> Result<(), crate::AgentError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn register_and_get() {
        let registry = AgentRegistry::new();
        let id = AgentId::new("test");

        let (actor_ref, handle) = AgentActor::spawn(
            Some("reg-test".to_string()),
            AgentActor,
            (
                AgentConfig::default(),
                Box::new(NoopBehavior) as Box<dyn AgentBehavior>,
            ),
        )
        .await
        .unwrap();

        registry.register(id.clone(), actor_ref.clone()).await;

        assert!(registry.get(&id).await.is_some());
        assert!(registry.get(&AgentId::new("nonexistent")).await.is_none());
        assert_eq!(registry.count().await, 1);

        actor_ref.stop(None);
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn list_ids() {
        let registry = AgentRegistry::new();

        let (ref1, h1) = AgentActor::spawn(
            Some("list-1".to_string()),
            AgentActor,
            (
                AgentConfig {
                    id: AgentId::new("a"),
                    ..AgentConfig::default()
                },
                Box::new(NoopBehavior) as Box<dyn AgentBehavior>,
            ),
        )
        .await
        .unwrap();

        let (ref2, h2) = AgentActor::spawn(
            Some("list-2".to_string()),
            AgentActor,
            (
                AgentConfig {
                    id: AgentId::new("b"),
                    ..AgentConfig::default()
                },
                Box::new(NoopBehavior) as Box<dyn AgentBehavior>,
            ),
        )
        .await
        .unwrap();

        registry.register(AgentId::new("a"), ref1.clone()).await;
        registry.register(AgentId::new("b"), ref2.clone()).await;

        let mut ids: Vec<String> = registry
            .list_ids()
            .await
            .into_iter()
            .map(|id| id.0)
            .collect();
        ids.sort();
        assert_eq!(ids, vec!["a", "b"]);

        ref1.stop(None);
        ref2.stop(None);
        h1.await.unwrap();
        h2.await.unwrap();
    }

    #[tokio::test]
    async fn is_alive_tracks_actor_lifecycle() {
        let registry = AgentRegistry::new();
        let id = AgentId::new("liveness");

        let (actor_ref, handle) = AgentActor::spawn(
            Some("liveness-test".to_string()),
            AgentActor,
            (
                AgentConfig::default(),
                Box::new(NoopBehavior) as Box<dyn AgentBehavior>,
            ),
        )
        .await
        .unwrap();
        registry.register(id.clone(), actor_ref.clone()).await;

        // Unknown agent is never alive; a running one is.
        assert!(!registry.is_alive(&AgentId::new("ghost")).await);
        assert!(registry.is_alive(&id).await);

        // Once stopped, the registry reports it dead (the supervision signal).
        actor_ref.stop(None);
        handle.await.unwrap();
        assert!(!registry.is_alive(&id).await);
    }

    #[tokio::test]
    async fn send_to_nonexistent_fails() {
        let registry = AgentRegistry::new();
        let (tx, _rx) = tokio::sync::oneshot::channel();
        let result = registry
            .send(
                &AgentId::new("ghost"),
                AgentMessage::Execute {
                    input: AgentInput::text("hi"),
                    reply: tx,
                    sink: None,
                },
            )
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn remove_agent() {
        let registry = AgentRegistry::new();
        let id = AgentId::new("removable");

        let (actor_ref, handle) = AgentActor::spawn(
            Some("remove-test".to_string()),
            AgentActor,
            (
                AgentConfig::default(),
                Box::new(NoopBehavior) as Box<dyn AgentBehavior>,
            ),
        )
        .await
        .unwrap();

        registry.register(id.clone(), actor_ref.clone()).await;
        assert_eq!(registry.count().await, 1);

        registry.remove(&id).await;
        assert_eq!(registry.count().await, 0);

        actor_ref.stop(None);
        handle.await.unwrap();
    }
}
