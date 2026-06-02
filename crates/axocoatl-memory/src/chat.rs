//! Lightweight chat conversations — the directoryless cousin of `Session`.
//!
//! A `Chat` is "agent + history" — no working directory, no sandbox, no
//! dev-container. It's the surface for the Chat tab (talk-only with an
//! agent) while [`crate::session::SessionMemory`] backs Sessions (build
//! in a sandboxed directory).
//!
//! Why a separate store? `Session` carries `working_dir`, `mode`, `image`,
//! `exposed_ports`, `post_create_commands` — all wrong-shape for casual
//! chats. Forcing every field optional would scatter conditionals across
//! every callsite. The persistence pattern (atomic temp+rename JSON write)
//! is copied from `crates/axocoatl-session/src/lib.rs`.
//!
//! Branching: chats fork from any message index. The new chat copies the
//! prefix and the user's edited message, then resumes from there. The
//! parent stays intact. Checkpoints in [`crate::checkpoint`] are keyed by
//! agent_id (one timeline per agent), and we deliberately do *not* touch
//! that — chat history lives in the chat file.

use crate::session::StoredMessage;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// Errors from chat-store operations.
#[derive(Debug, thiserror::Error)]
pub enum ChatError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("chat not found: {0}")]
    NotFound(String),
    #[error("invalid index {idx} into chat with {len} messages")]
    BadIndex { idx: usize, len: usize },
    #[error("invalid: {0}")]
    Invalid(String),
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn gen_id() -> String {
    // Mirrors SessionStore's id convention (see `crates/axocoatl-session`).
    format!("chat-{}", uuid::Uuid::new_v4())
}

/// One attachment reference on a chat. The actual bytes + metadata live in
/// the content-addressed [`crate::files::FileStore`]; this struct is just the
/// "this chat references that file" relationship plus chat-local state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatAttachment {
    /// SHA-256 of the file's bytes. Resolves to a `FileEntry` in FileStore.
    pub file_id: String,
    /// Pinned attachments survive turn drains — they re-attach on every
    /// turn until the user removes them. Use for "the spec we're discussing".
    #[serde(default)]
    pub pinned: bool,
    /// Unix-seconds when this reference was created.
    pub added_at: u64,
}

/// A single chat conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Chat {
    pub id: String,
    /// User-facing name. Defaults to first user message snippet; rename anytime.
    pub name: String,
    /// Which agent answers in this chat.
    pub agent_id: String,
    /// Per-chat system prompt override. `None` = use the agent's default.
    #[serde(default)]
    pub system_override: Option<String>,
    /// Per-chat model override (e.g. `"llama3.2:1b"`). `None` = agent default.
    #[serde(default)]
    pub model_override: Option<String>,
    /// Pinned chats float to the top of the sidebar.
    #[serde(default)]
    pub starred: bool,
    /// If forked from another chat, points at the parent.
    #[serde(default)]
    pub parent_id: Option<String>,
    /// Index in the parent's `messages` where this chat diverged.
    #[serde(default)]
    pub forked_at_message: Option<usize>,
    /// The transcript. Append-only during normal use; fork() copies a prefix.
    #[serde(default)]
    pub messages: Vec<StoredMessage>,
    /// Files referenced by this chat. Each entry points at an immutable
    /// `FileEntry` in the FileStore by content hash. After every turn, the
    /// non-`pinned` entries get drained; pinned entries re-attach to every
    /// future turn until the user un-pins them.
    #[serde(default)]
    pub attachments: Vec<ChatAttachment>,
    pub created_at: u64,
    pub last_active: u64,
}

impl Chat {
    fn new(agent_id: String, name: String) -> Self {
        let now = now_secs();
        Self {
            id: gen_id(),
            name,
            agent_id,
            system_override: None,
            model_override: None,
            starred: false,
            parent_id: None,
            forked_at_message: None,
            messages: Vec::new(),
            attachments: Vec::new(),
            created_at: now,
            last_active: now,
        }
    }
}

/// JSON-on-disk chat store. One file per chat at `{dir}/{chat_id}.json`.
pub struct ChatStore {
    dir: PathBuf,
    chats: HashMap<String, Chat>,
}

impl ChatStore {
    /// Open the store rooted at `dir` (created if absent). Call [`load_all`]
    /// to read existing chats back in.
    ///
    /// [`load_all`]: ChatStore::load_all
    pub fn new(dir: impl Into<PathBuf>) -> Result<Self, ChatError> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir)?;
        Ok(Self {
            dir,
            chats: HashMap::new(),
        })
    }

    /// Load every persisted chat from disk. Malformed files are skipped.
    pub fn load_all(&mut self) -> Result<(), ChatError> {
        for entry in std::fs::read_dir(&self.dir)? {
            let path = entry?.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            if let Ok(bytes) = std::fs::read(&path) {
                if let Ok(chat) = serde_json::from_slice::<Chat>(&bytes) {
                    self.chats.insert(chat.id.clone(), chat);
                }
            }
        }
        Ok(())
    }

    /// Create a fresh chat.
    pub fn create(
        &mut self,
        agent_id: impl Into<String>,
        name: impl Into<String>,
    ) -> Result<Chat, ChatError> {
        let name = {
            let n = name.into();
            let n = n.trim();
            if n.is_empty() {
                "New chat".to_string()
            } else {
                n.to_string()
            }
        };
        let chat = Chat::new(agent_id.into(), name);
        self.persist(&chat)?;
        self.chats.insert(chat.id.clone(), chat.clone());
        Ok(chat)
    }

    /// Fetch a chat by id.
    pub fn get(&self, id: &str) -> Option<Chat> {
        self.chats.get(id).cloned()
    }

    /// All chats — starred first, then newest `last_active` first.
    pub fn list(&self) -> Vec<Chat> {
        let mut v: Vec<Chat> = self.chats.values().cloned().collect();
        v.sort_by(|a, b| {
            b.starred
                .cmp(&a.starred)
                .then(b.last_active.cmp(&a.last_active))
        });
        v
    }

    /// Append one message to a chat and bump `last_active`.
    pub fn append_message(&mut self, id: &str, msg: StoredMessage) -> Result<(), ChatError> {
        let c = self
            .chats
            .get_mut(id)
            .ok_or_else(|| ChatError::NotFound(id.to_string()))?;
        c.messages.push(msg);
        c.last_active = now_secs();
        let snap = c.clone();
        self.persist(&snap)
    }

    /// Add a file reference to a chat. Idempotent on file_id — re-adding
    /// the same file (e.g. dragging it twice) doesn't duplicate the row.
    pub fn add_attachment(&mut self, chat_id: &str, file_id: &str) -> Result<(), ChatError> {
        let c = self
            .chats
            .get_mut(chat_id)
            .ok_or_else(|| ChatError::NotFound(chat_id.to_string()))?;
        if c.attachments.iter().any(|a| a.file_id == file_id) {
            return Ok(());
        }
        c.attachments.push(ChatAttachment {
            file_id: file_id.to_string(),
            pinned: false,
            added_at: now_secs(),
        });
        c.last_active = now_secs();
        let snap = c.clone();
        self.persist(&snap)
    }

    /// Remove an attachment reference from a chat. The underlying `FileEntry`
    /// in FileStore is NOT touched — other chats may still reference it.
    pub fn remove_attachment(&mut self, chat_id: &str, file_id: &str) -> Result<bool, ChatError> {
        let c = self
            .chats
            .get_mut(chat_id)
            .ok_or_else(|| ChatError::NotFound(chat_id.to_string()))?;
        let before = c.attachments.len();
        c.attachments.retain(|a| a.file_id != file_id);
        let removed = c.attachments.len() < before;
        if removed {
            let snap = c.clone();
            self.persist(&snap)?;
        }
        Ok(removed)
    }

    /// Pin or unpin an attachment. Pinned attachments survive turn drains.
    pub fn set_attachment_pinned(
        &mut self,
        chat_id: &str,
        file_id: &str,
        pinned: bool,
    ) -> Result<bool, ChatError> {
        let c = self
            .chats
            .get_mut(chat_id)
            .ok_or_else(|| ChatError::NotFound(chat_id.to_string()))?;
        let Some(a) = c.attachments.iter_mut().find(|a| a.file_id == file_id) else {
            return Ok(false);
        };
        a.pinned = pinned;
        let snap = c.clone();
        self.persist(&snap)?;
        Ok(true)
    }

    /// Drain non-pinned attachments after a turn fires. Returns the list of
    /// file_ids that were sent on this turn (both drained AND retained pinned
    /// ones, in their original order) so the caller can build the LLM payload.
    /// Pinned entries stay in `chat.attachments` for the next turn.
    pub fn consume_attachments_for_turn(
        &mut self,
        chat_id: &str,
    ) -> Result<Vec<ChatAttachment>, ChatError> {
        let c = self
            .chats
            .get_mut(chat_id)
            .ok_or_else(|| ChatError::NotFound(chat_id.to_string()))?;
        let sent_this_turn = c.attachments.clone();
        // Retain pinned-only.
        c.attachments.retain(|a| a.pinned);
        if sent_this_turn.iter().any(|a| !a.pinned) {
            let snap = c.clone();
            self.persist(&snap)?;
        }
        Ok(sent_this_turn)
    }

    /// Pop the last message (e.g. before a regenerate). Returns the dropped one.
    pub fn pop_last(&mut self, id: &str) -> Result<Option<StoredMessage>, ChatError> {
        let c = self
            .chats
            .get_mut(id)
            .ok_or_else(|| ChatError::NotFound(id.to_string()))?;
        let popped = c.messages.pop();
        if popped.is_some() {
            c.last_active = now_secs();
            let snap = c.clone();
            self.persist(&snap)?;
        }
        Ok(popped)
    }

    /// Rename a chat in place.
    pub fn rename(&mut self, id: &str, new_name: impl Into<String>) -> Result<Chat, ChatError> {
        let new_name = new_name.into();
        let name = new_name.trim();
        if name.is_empty() {
            return Err(ChatError::Invalid("name is empty".to_string()));
        }
        let c = self
            .chats
            .get_mut(id)
            .ok_or_else(|| ChatError::NotFound(id.to_string()))?;
        c.name = name.to_string();
        let snap = c.clone();
        self.persist(&snap)?;
        Ok(snap)
    }

    /// Star/unstar a chat.
    pub fn star(&mut self, id: &str, starred: bool) -> Result<Chat, ChatError> {
        let c = self
            .chats
            .get_mut(id)
            .ok_or_else(|| ChatError::NotFound(id.to_string()))?;
        c.starred = starred;
        let snap = c.clone();
        self.persist(&snap)?;
        Ok(snap)
    }

    /// Set or clear the per-chat system prompt and model overrides.
    pub fn set_overrides(
        &mut self,
        id: &str,
        system_override: Option<String>,
        model_override: Option<String>,
    ) -> Result<Chat, ChatError> {
        let c = self
            .chats
            .get_mut(id)
            .ok_or_else(|| ChatError::NotFound(id.to_string()))?;
        c.system_override = system_override;
        c.model_override = model_override;
        let snap = c.clone();
        self.persist(&snap)?;
        Ok(snap)
    }

    /// Fork: create a new chat from `parent_id` with messages `[0..truncate_at]`
    /// optionally replacing the message at `truncate_at` with `replacement`.
    /// Common case: user edits their last turn — `truncate_at` is its index
    /// and `replacement` is the new user message.
    pub fn fork(
        &mut self,
        parent_id: &str,
        truncate_at: usize,
        replacement: Option<StoredMessage>,
    ) -> Result<Chat, ChatError> {
        let parent = self
            .chats
            .get(parent_id)
            .ok_or_else(|| ChatError::NotFound(parent_id.to_string()))?
            .clone();
        if truncate_at > parent.messages.len() {
            return Err(ChatError::BadIndex {
                idx: truncate_at,
                len: parent.messages.len(),
            });
        }
        let prefix: Vec<StoredMessage> = parent.messages[..truncate_at].to_vec();
        let mut child = Chat::new(parent.agent_id.clone(), format!("{} (fork)", parent.name));
        child.system_override = parent.system_override.clone();
        child.model_override = parent.model_override.clone();
        child.parent_id = Some(parent.id.clone());
        child.forked_at_message = Some(truncate_at);
        child.messages = prefix;
        if let Some(r) = replacement {
            child.messages.push(r);
        }
        self.persist(&child)?;
        self.chats.insert(child.id.clone(), child.clone());
        Ok(child)
    }

    /// Delete a chat entirely (from memory and disk).
    pub fn remove(&mut self, id: &str) -> Result<(), ChatError> {
        if self.chats.remove(id).is_none() {
            return Err(ChatError::NotFound(id.to_string()));
        }
        let path = self.dir.join(format!("{id}.json"));
        if path.exists() {
            std::fs::remove_file(path)?;
        }
        Ok(())
    }

    /// Naive full-text search over all chats. Case-insensitive substring match
    /// across name + every message content. Returns chats newest-first.
    pub fn search(&self, query: &str) -> Vec<Chat> {
        let q = query.trim().to_lowercase();
        if q.is_empty() {
            return self.list();
        }
        let mut hits: Vec<Chat> = self
            .chats
            .values()
            .filter(|c| {
                c.name.to_lowercase().contains(&q)
                    || c.messages
                        .iter()
                        .any(|m| m.content.to_lowercase().contains(&q))
            })
            .cloned()
            .collect();
        hits.sort_by_key(|x| std::cmp::Reverse(x.last_active));
        hits
    }

    /// Number of chats held.
    pub fn len(&self) -> usize {
        self.chats.len()
    }

    /// True iff there are no chats.
    pub fn is_empty(&self) -> bool {
        self.chats.is_empty()
    }

    /// Atomically write one chat to `{dir}/{id}.json` (temp + rename).
    fn persist(&self, chat: &Chat) -> Result<(), ChatError> {
        let path = self.dir.join(format!("{}.json", chat.id));
        let tmp = path.with_extension("json.tmp");
        let bytes = serde_json::to_vec_pretty(chat)?;
        std::fs::write(&tmp, &bytes)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axocoatl_core::MessageRole;
    use tempfile::tempdir;

    fn msg(role: MessageRole, content: &str) -> StoredMessage {
        StoredMessage {
            role,
            content: content.into(),
            timestamp: now_secs(),
            token_count: content.split_whitespace().count(),
        }
    }

    #[test]
    fn create_append_list_persistence_roundtrip() {
        let data = tempdir().unwrap();
        let mut store = ChatStore::new(data.path().join("chats")).unwrap();
        let c = store.create("secretary", "First chat").unwrap();
        store
            .append_message(&c.id, msg(MessageRole::User, "hello"))
            .unwrap();
        store
            .append_message(&c.id, msg(MessageRole::Assistant, "hi back"))
            .unwrap();
        assert_eq!(store.get(&c.id).unwrap().messages.len(), 2);

        let mut reopened = ChatStore::new(data.path().join("chats")).unwrap();
        reopened.load_all().unwrap();
        assert_eq!(reopened.get(&c.id).unwrap().messages.len(), 2);
    }

    #[test]
    fn fork_preserves_prefix_and_independence() {
        let data = tempdir().unwrap();
        let mut store = ChatStore::new(data.path().join("chats")).unwrap();
        let parent = store.create("secretary", "Parent").unwrap();
        for i in 0..4 {
            store
                .append_message(&parent.id, msg(MessageRole::User, &format!("u{i}")))
                .unwrap();
        }
        // Fork at index 2 (keeping u0, u1) with an edited new message.
        let child = store
            .fork(&parent.id, 2, Some(msg(MessageRole::User, "edited")))
            .unwrap();
        assert_eq!(child.messages.len(), 3);
        assert_eq!(child.messages[0].content, "u0");
        assert_eq!(child.messages[1].content, "u1");
        assert_eq!(child.messages[2].content, "edited");
        assert_eq!(child.parent_id.as_deref(), Some(parent.id.as_str()));
        assert_eq!(child.forked_at_message, Some(2));
        // Parent untouched.
        assert_eq!(store.get(&parent.id).unwrap().messages.len(), 4);
    }

    #[test]
    fn fork_out_of_range_errors() {
        let data = tempdir().unwrap();
        let mut store = ChatStore::new(data.path().join("chats")).unwrap();
        let p = store.create("secretary", "P").unwrap();
        store
            .append_message(&p.id, msg(MessageRole::User, "x"))
            .unwrap();
        let err = store.fork(&p.id, 99, None).unwrap_err();
        matches!(err, ChatError::BadIndex { .. });
    }

    #[test]
    fn search_matches_name_and_content() {
        let data = tempdir().unwrap();
        let mut store = ChatStore::new(data.path().join("chats")).unwrap();
        let a = store.create("secretary", "Q4 planning").unwrap();
        let b = store.create("secretary", "Random").unwrap();
        store
            .append_message(&b.id, msg(MessageRole::User, "we discussed Q4 ideas here"))
            .unwrap();
        let hits = store.search("q4");
        let ids: Vec<&str> = hits.iter().map(|c| c.id.as_str()).collect();
        assert!(ids.contains(&a.id.as_str()));
        assert!(ids.contains(&b.id.as_str()));
    }

    #[test]
    fn starred_chats_sort_to_top() {
        let data = tempdir().unwrap();
        let mut store = ChatStore::new(data.path().join("chats")).unwrap();
        let a = store.create("secretary", "old").unwrap();
        // Bump above whole-second timestamp granularity so `b` is verifiably newer.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let b = store.create("secretary", "newer").unwrap();
        assert_eq!(store.list()[0].id, b.id);
        store.star(&a.id, true).unwrap();
        assert_eq!(store.list()[0].id, a.id);
    }

    #[test]
    fn pop_last_returns_and_persists() {
        let data = tempdir().unwrap();
        let mut store = ChatStore::new(data.path().join("chats")).unwrap();
        let c = store.create("secretary", "x").unwrap();
        store
            .append_message(&c.id, msg(MessageRole::User, "a"))
            .unwrap();
        store
            .append_message(&c.id, msg(MessageRole::Assistant, "b"))
            .unwrap();
        let popped = store.pop_last(&c.id).unwrap().unwrap();
        assert_eq!(popped.content, "b");
        assert_eq!(store.get(&c.id).unwrap().messages.len(), 1);
    }
}
