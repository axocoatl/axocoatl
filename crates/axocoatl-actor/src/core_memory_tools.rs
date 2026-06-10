//! Agent-callable core-memory edit tools (MemGPT/Letta-style).
//!
//! Each tool holds clones of the behavior's per-agent [`CoreMemoryStore`] plus
//! the shared blocks this agent may edit — the *same* `Arc`s the behavior renders
//! from — so an edit is visible the moment the next request is built (same-turn).
//! They are owned by the behavior (not the shared `ToolExecutor`, which can't
//! carry agent identity), exactly like the recall tools.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::RwLock;

use axocoatl_memory::{CoreMemoryStore, MemoryError, SharedBlock};
use axocoatl_tools::{BuiltinTool, ToolError};

pub const CORE_MEMORY_APPEND: &str = "core_memory_append";
pub const CORE_MEMORY_REPLACE: &str = "core_memory_replace";
pub const CORE_MEMORY_SET: &str = "core_memory_set";

/// The handles every core-memory tool shares: the per-agent store + the shared
/// blocks this agent is allowed to edit (label → cross-agent handle).
#[derive(Clone)]
pub struct CoreMemoryHandles {
    pub store: Arc<RwLock<CoreMemoryStore>>,
    pub shared: HashMap<String, SharedBlock>,
}

enum Edit<'a> {
    Append(&'a str),
    Replace(&'a str, &'a str),
    Set(&'a str),
}

fn apply(block: &mut axocoatl_memory::MemoryBlock, edit: &Edit) -> Result<(), MemoryError> {
    match edit {
        Edit::Append(t) => block.append(t),
        Edit::Replace(old, new) => block.replace(old, new),
        Edit::Set(v) => block.set(v),
    }
}

fn exec_err(tool: &str, e: MemoryError) -> ToolError {
    ToolError::ExecutionFailed {
        tool: tool.to_string(),
        reason: e.to_string(),
    }
}

fn arg_str<'a>(args: &'a Value, key: &str, tool: &str) -> Result<&'a str, ToolError> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ToolError::InvalidArgs {
            tool: tool.to_string(),
            reason: format!("missing or empty '{key}'"),
        })
}

/// Apply an edit to a block — a shared block (if the label is shared) or the
/// agent's local store — persisting after the change.
async fn apply_edit(
    handles: &CoreMemoryHandles,
    tool: &str,
    label: &str,
    edit: Edit<'_>,
) -> Result<Value, ToolError> {
    if let Some(shared) = handles.shared.get(label) {
        {
            let mut b = shared.block.write().await;
            apply(&mut b, &edit).map_err(|e| exec_err(tool, e))?;
        }
        shared.persist().await.map_err(|e| exec_err(tool, e))?;
        let chars = shared.block.read().await.value.chars().count();
        return Ok(json!({ "ok": true, "block": label, "chars": chars }));
    }

    let mut store = handles.store.write().await;
    let block = store
        .block_mut(label)
        .ok_or_else(|| ToolError::InvalidArgs {
            tool: tool.to_string(),
            reason: format!("no core-memory block named '{label}'"),
        })?;
    apply(block, &edit).map_err(|e| exec_err(tool, e))?;
    let chars = store
        .block(label)
        .map(|b| b.value.chars().count())
        .unwrap_or(0);
    store.save().await.map_err(|e| exec_err(tool, e))?;
    Ok(json!({ "ok": true, "block": label, "chars": chars }))
}

/// `core_memory_append` — add text to a block.
pub struct CoreMemoryAppendTool {
    handles: CoreMemoryHandles,
}
impl CoreMemoryAppendTool {
    pub fn new(handles: CoreMemoryHandles) -> Self {
        Self { handles }
    }
}

#[async_trait]
impl BuiltinTool for CoreMemoryAppendTool {
    fn description(&self) -> &str {
        "Append a line to one of your core-memory blocks (persona, human, project, …). Use this \
         to record a new durable fact about yourself, the user, or the project. Keep it concise — \
         blocks have a character limit; if you hit it, use core_memory_replace to condense."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "block": { "type": "string", "description": "Block label to edit" },
                "text": { "type": "string", "description": "Text to append" }
            },
            "required": ["block", "text"]
        })
    }
    async fn execute(&self, arguments: Value) -> Result<Value, ToolError> {
        let label = arg_str(&arguments, "block", CORE_MEMORY_APPEND)?;
        let text = arg_str(&arguments, "text", CORE_MEMORY_APPEND)?;
        apply_edit(&self.handles, CORE_MEMORY_APPEND, label, Edit::Append(text)).await
    }
}

/// `core_memory_replace` — find-and-replace within a block.
pub struct CoreMemoryReplaceTool {
    handles: CoreMemoryHandles,
}
impl CoreMemoryReplaceTool {
    pub fn new(handles: CoreMemoryHandles) -> Self {
        Self { handles }
    }
}

#[async_trait]
impl BuiltinTool for CoreMemoryReplaceTool {
    fn description(&self) -> &str {
        "Replace the first occurrence of some text within a core-memory block. Use to correct or \
         update an existing fact, or to shorten a block that is near its limit. Errors if the old \
         text is not present."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "block": { "type": "string", "description": "Block label to edit" },
                "old": { "type": "string", "description": "Existing text to replace" },
                "new": { "type": "string", "description": "Replacement text" }
            },
            "required": ["block", "old", "new"]
        })
    }
    async fn execute(&self, arguments: Value) -> Result<Value, ToolError> {
        let label = arg_str(&arguments, "block", CORE_MEMORY_REPLACE)?;
        let old = arg_str(&arguments, "old", CORE_MEMORY_REPLACE)?;
        // `new` may legitimately be empty (deletion), so don't reject empties.
        let new = arguments.get("new").and_then(|v| v.as_str()).unwrap_or("");
        apply_edit(
            &self.handles,
            CORE_MEMORY_REPLACE,
            label,
            Edit::Replace(old, new),
        )
        .await
    }
}

/// `core_memory_set` — overwrite a block's contents.
pub struct CoreMemorySetTool {
    handles: CoreMemoryHandles,
}
impl CoreMemorySetTool {
    pub fn new(handles: CoreMemoryHandles) -> Self {
        Self { handles }
    }
}

#[async_trait]
impl BuiltinTool for CoreMemorySetTool {
    fn description(&self) -> &str {
        "Overwrite a core-memory block's entire contents. Use sparingly — prefer \
         core_memory_append / core_memory_replace so existing facts are preserved."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "block": { "type": "string", "description": "Block label to overwrite" },
                "value": { "type": "string", "description": "New full contents" }
            },
            "required": ["block", "value"]
        })
    }
    async fn execute(&self, arguments: Value) -> Result<Value, ToolError> {
        let label = arg_str(&arguments, "block", CORE_MEMORY_SET)?;
        let value = arguments
            .get("value")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        apply_edit(&self.handles, CORE_MEMORY_SET, label, Edit::Set(value)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axocoatl_memory::{MemoryBlock, SharedBlockRegistry};

    fn handles_with(label: &str, limit: usize, dir: &std::path::Path) -> CoreMemoryHandles {
        let mut store = CoreMemoryStore::new("a", dir.join("a.json"));
        store.ensure_block(MemoryBlock::new(label, limit));
        CoreMemoryHandles {
            store: Arc::new(RwLock::new(store)),
            shared: HashMap::new(),
        }
    }

    #[tokio::test]
    async fn append_replace_set_persist() {
        let dir = tempfile::tempdir().unwrap();
        let h = handles_with("human", 100, dir.path());

        CoreMemoryAppendTool::new(h.clone())
            .execute(json!({ "block": "human", "text": "name: Alice" }))
            .await
            .unwrap();
        assert_eq!(
            h.store.read().await.block("human").unwrap().value,
            "name: Alice"
        );

        CoreMemoryReplaceTool::new(h.clone())
            .execute(json!({ "block": "human", "old": "Alice", "new": "Bob" }))
            .await
            .unwrap();
        assert!(h
            .store
            .read()
            .await
            .block("human")
            .unwrap()
            .value
            .contains("Bob"));

        CoreMemorySetTool::new(h.clone())
            .execute(json!({ "block": "human", "value": "reset" }))
            .await
            .unwrap();
        assert_eq!(h.store.read().await.block("human").unwrap().value, "reset");

        // Persisted to disk (a fresh store loads it back).
        let mut reloaded = CoreMemoryStore::new("a", dir.path().join("a.json"));
        reloaded.load().await.unwrap();
        assert_eq!(reloaded.block("human").unwrap().value, "reset");
    }

    #[tokio::test]
    async fn errors_surface_to_model() {
        let dir = tempfile::tempdir().unwrap();
        let h = handles_with("human", 10, dir.path());
        let append = CoreMemoryAppendTool::new(h.clone());
        // Over-limit → error.
        assert!(append
            .execute(json!({ "block": "human", "text": "way too long for ten" }))
            .await
            .is_err());
        // Unknown block → error.
        assert!(append
            .execute(json!({ "block": "nope", "text": "x" }))
            .await
            .is_err());
        // Missing args → error.
        assert!(append.execute(json!({ "block": "human" })).await.is_err());
        // Replace of absent text → error.
        assert!(CoreMemoryReplaceTool::new(h)
            .execute(json!({ "block": "human", "old": "zzz", "new": "y" }))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn edits_a_shared_block() {
        let dir = tempfile::tempdir().unwrap();
        let mut reg = SharedBlockRegistry::new(dir.path().join("shared"));
        let team = reg.ensure(MemoryBlock::new("team", 0)).await;
        let mut shared = HashMap::new();
        shared.insert("team".to_string(), team.clone());
        let store = CoreMemoryStore::new("a", dir.path().join("a.json"));
        let h = CoreMemoryHandles {
            store: Arc::new(RwLock::new(store)),
            shared,
        };
        CoreMemoryAppendTool::new(h)
            .execute(json!({ "block": "team", "text": "release is Friday" }))
            .await
            .unwrap();
        assert!(team.block.read().await.value.contains("Friday"));
    }
}
