//! Host-owned checkpoint storage for one exact controlled activation.
//!
//! The host binds this port to immutable canonical input and retained storage
//! reservations. Restoring `None` explicitly selects an empty conversation;
//! it must never mean that expected checkpoint bytes could not be found.
//! Staging records evidence only. Canonical acceptance and promotion remain
//! host decisions after the actor's outcome and all required evidence persist.

use async_trait::async_trait;
use axocoatl_memory::AgentCheckpoint;

#[async_trait]
pub trait ActivationCheckpointPort: Send + Sync {
    /// Load only the activation's exact starting savepoint, never a latest file.
    async fn restore(&self) -> Result<Option<AgentCheckpoint>, String>;

    /// Acknowledge the complete candidate durably before actor success escapes.
    /// A failed/cancelled run may stage evidence but cannot thereby accept it.
    async fn stage(&self, checkpoint: &AgentCheckpoint) -> Result<(), String>;

    /// The host's reserved encoded candidate capacity. The actor also enforces
    /// the memory crate's hard ceiling. Zero refuses startup before paid work.
    fn maximum_checkpoint_bytes(&self) -> usize {
        axocoatl_memory::MAX_CHECKPOINT_BYTES
    }
}
