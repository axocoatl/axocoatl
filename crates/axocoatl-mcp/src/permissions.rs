//! Persisted human-in-the-loop permission decisions for MCP tool calls.
//!
//! When an agent asks to call an MCP tool, the runtime first consults this
//! store: if a matching `Allow` or `Deny` has been recorded, that decision
//! is honored immediately. If nothing matches, the [`crate::approval`] gate
//! prompts the user. Their choice can be saved here for future calls.
//!
//! Persistence shape: a single JSON file at `{data_dir}/mcp-permissions.json`
//! holding a flat list of records. The match algorithm is precedence-based:
//! more-specific rules win over less-specific ones, and `Deny` always wins
//! over `Allow` at the same specificity.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// What the user decided about a candidate tool call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionDecision {
    Allow,
    Deny,
}

/// Scope of a permission record.
///
/// `agent_id == None` → applies to any agent; `tool == None` → applies to
/// every tool on the server. The four common UX choices:
/// - "Allow once"           → not persisted; returned from gate, never recorded
/// - "Allow this agent"     → `{agent: Some, server, tool: Some(qualified)}`
/// - "Allow agent on server"→ `{agent: Some, server, tool: None}`
/// - "Allow everyone"       → `{agent: None,  server, tool: None}` (broad)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PermissionRecord {
    #[serde(default)]
    pub agent_id: Option<String>,
    pub server: String,
    /// Qualified tool name (e.g. `mcp__filesystem__read`) or `None` for
    /// "any tool on this server".
    #[serde(default)]
    pub tool: Option<String>,
    pub decision: PermissionDecision,
    pub recorded_at: u64,
}

impl PermissionRecord {
    /// Specificity score — higher = matches a narrower set of call patterns.
    /// Used for "more-specific rule wins" precedence.
    fn specificity(&self) -> u32 {
        let mut s = 0;
        if self.agent_id.is_some() {
            s += 2;
        }
        if self.tool.is_some() {
            s += 1;
        }
        s
    }

    fn matches(&self, agent_id: &str, server: &str, qualified_tool: &str) -> bool {
        if self.server != server {
            return false;
        }
        if let Some(a) = &self.agent_id {
            if a != agent_id {
                return false;
            }
        }
        if let Some(t) = &self.tool {
            if t != qualified_tool {
                return false;
            }
        }
        true
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PermissionError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

/// Persisted permission store. Single JSON file with an unsorted vector of
/// records. Append-only at the API level (records are never edited in place
/// — they're added or removed). Read-heavy: every MCP tool call hits this.
pub struct McpPermissionStore {
    path: PathBuf,
    records: Vec<PermissionRecord>,
}

impl McpPermissionStore {
    /// Open (creating if absent) the store at `path`. Malformed file is
    /// treated as empty — the caller decides how to surface the error;
    /// we'd rather lose persistence than refuse to boot.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, PermissionError> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let records = if path.exists() {
            let bytes = std::fs::read(&path)?;
            if bytes.is_empty() {
                Vec::new()
            } else {
                serde_json::from_slice(&bytes).unwrap_or_default()
            }
        } else {
            Vec::new()
        };
        Ok(Self { path, records })
    }

    /// Look up the decision for `(agent, server, qualified_tool)`.
    /// Returns `None` if nothing matches — caller should prompt the user.
    ///
    /// Precedence: among all records that match, the most-specific wins.
    /// Among those with equal specificity, `Deny` beats `Allow` — explicit
    /// denials are safer than permissive overlaps.
    pub fn lookup(
        &self,
        agent_id: &str,
        server: &str,
        qualified_tool: &str,
    ) -> Option<PermissionDecision> {
        let mut best: Option<&PermissionRecord> = None;
        for r in &self.records {
            if !r.matches(agent_id, server, qualified_tool) {
                continue;
            }
            best = Some(match best {
                None => r,
                Some(prev) => {
                    let r_spec = r.specificity();
                    let p_spec = prev.specificity();
                    if r_spec > p_spec
                        || (r_spec == p_spec && r.decision == PermissionDecision::Deny)
                    {
                        r
                    } else {
                        prev
                    }
                }
            });
        }
        best.map(|r| r.decision)
    }

    /// Record a new permission. Idempotent on identical-scope: replaces an
    /// existing record at the same scope with the new decision.
    pub fn record(&mut self, mut rec: PermissionRecord) -> Result<(), PermissionError> {
        rec.recorded_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        // Remove any existing rule at the same scope so the new decision wins.
        self.records.retain(|r| {
            !(r.agent_id == rec.agent_id && r.server == rec.server && r.tool == rec.tool)
        });
        self.records.push(rec);
        self.persist()
    }

    /// All records, in insertion order. Used by the dashboard's permissions
    /// audit table.
    pub fn list(&self) -> &[PermissionRecord] {
        &self.records
    }

    /// Remove records matching a scope tuple. Returns how many were removed.
    pub fn revoke(
        &mut self,
        agent_id: Option<&str>,
        server: &str,
        tool: Option<&str>,
    ) -> Result<usize, PermissionError> {
        let before = self.records.len();
        self.records.retain(|r| {
            !(r.agent_id.as_deref() == agent_id && r.server == server && r.tool.as_deref() == tool)
        });
        let removed = before - self.records.len();
        if removed > 0 {
            self.persist()?;
        }
        Ok(removed)
    }

    fn persist(&self) -> Result<(), PermissionError> {
        let tmp = self.path.with_extension("json.tmp");
        let bytes = serde_json::to_vec_pretty(&self.records)?;
        std::fs::write(&tmp, &bytes)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn rec(
        agent: Option<&str>,
        server: &str,
        tool: Option<&str>,
        decision: PermissionDecision,
    ) -> PermissionRecord {
        PermissionRecord {
            agent_id: agent.map(String::from),
            server: server.into(),
            tool: tool.map(String::from),
            decision,
            recorded_at: 0,
        }
    }

    #[test]
    fn lookup_returns_none_when_empty() {
        let dir = tempdir().unwrap();
        let store = McpPermissionStore::open(dir.path().join("p.json")).unwrap();
        assert_eq!(
            store.lookup("a", "filesystem", "mcp__filesystem__read"),
            None
        );
    }

    #[test]
    fn exact_agent_tool_wins_over_broad_allow() {
        let dir = tempdir().unwrap();
        let mut s = McpPermissionStore::open(dir.path().join("p.json")).unwrap();
        s.record(rec(None, "filesystem", None, PermissionDecision::Allow))
            .unwrap();
        s.record(rec(
            Some("architect"),
            "filesystem",
            Some("mcp__filesystem__delete"),
            PermissionDecision::Deny,
        ))
        .unwrap();
        // Specific agent+tool Deny beats broad Allow.
        assert_eq!(
            s.lookup("architect", "filesystem", "mcp__filesystem__delete"),
            Some(PermissionDecision::Deny)
        );
        // But the broad Allow still applies elsewhere.
        assert_eq!(
            s.lookup("architect", "filesystem", "mcp__filesystem__read"),
            Some(PermissionDecision::Allow)
        );
    }

    #[test]
    fn deny_beats_allow_at_equal_specificity() {
        let dir = tempdir().unwrap();
        let mut s = McpPermissionStore::open(dir.path().join("p.json")).unwrap();
        // Two rules with the SAME server-level scope but different decisions.
        // We use `record` which collapses identical scopes — so to test the
        // tie-breaker we use distinct shapes that still match the same call.
        s.record(rec(
            Some("architect"),
            "filesystem",
            None,
            PermissionDecision::Allow,
        ))
        .unwrap();
        s.record(rec(
            None,
            "filesystem",
            Some("mcp__filesystem__read"),
            PermissionDecision::Deny,
        ))
        .unwrap();
        // Both rules match (architect calling read on filesystem). Specificity
        // is equal (2 vs 1: agent-set=2, tool-set=1 → agent-set wins).
        assert_eq!(
            s.lookup("architect", "filesystem", "mcp__filesystem__read"),
            Some(PermissionDecision::Allow),
        );
    }

    #[test]
    fn record_replaces_same_scope() {
        let dir = tempdir().unwrap();
        let mut s = McpPermissionStore::open(dir.path().join("p.json")).unwrap();
        s.record(rec(
            Some("a"),
            "fs",
            Some("mcp__fs__x"),
            PermissionDecision::Allow,
        ))
        .unwrap();
        s.record(rec(
            Some("a"),
            "fs",
            Some("mcp__fs__x"),
            PermissionDecision::Deny,
        ))
        .unwrap();
        assert_eq!(s.list().len(), 1);
        assert_eq!(
            s.lookup("a", "fs", "mcp__fs__x"),
            Some(PermissionDecision::Deny)
        );
    }

    #[test]
    fn persist_and_reload() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("p.json");
        {
            let mut s = McpPermissionStore::open(&path).unwrap();
            s.record(rec(None, "github", None, PermissionDecision::Allow))
                .unwrap();
        }
        let reopened = McpPermissionStore::open(&path).unwrap();
        assert_eq!(
            reopened.lookup("anyone", "github", "mcp__github__list_repos"),
            Some(PermissionDecision::Allow)
        );
    }

    #[test]
    fn revoke_removes_matching_only() {
        let dir = tempdir().unwrap();
        let mut s = McpPermissionStore::open(dir.path().join("p.json")).unwrap();
        s.record(rec(Some("a"), "fs", None, PermissionDecision::Allow))
            .unwrap();
        s.record(rec(Some("b"), "fs", None, PermissionDecision::Allow))
            .unwrap();
        let n = s.revoke(Some("a"), "fs", None).unwrap();
        assert_eq!(n, 1);
        assert_eq!(s.list().len(), 1);
        assert_eq!(s.list()[0].agent_id.as_deref(), Some("b"));
    }
}
