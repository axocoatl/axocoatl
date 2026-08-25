//! Tier 4: Semantic memory — embedding-based retrieval of past context.
//!
//! Each stored memory is turned into a vector; recall finds the nearest
//! vectors by cosine similarity (an exact brute-force scan — at per-agent
//! memory scale that is sub-millisecond, so an ANN index would be needless
//! weight).
//!
//! Two embedding backends sit behind one [`Embedder`] seam:
//!
//! * **Neural** (default) — `all-MiniLM-L6-v2` run with Candle (pure-Rust, no
//!   ONNX/C++). Similarity reflects *meaning*: "terse answers" and "concise
//!   responses" land close even with no shared words.
//! * **Hashed** (fallback) — signed feature hashing over word + char-trigram
//!   tokens. Similarity reflects *lexical overlap* only. Used when the neural
//!   model can't be loaded (e.g. offline first run), or with the
//!   `neural-embeddings` feature disabled.
//!
//! The active backend's id is recorded in the store file; if it changes, every
//! memory is re-embedded on load (the original text is always kept).

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use axocoatl_core::secure_fs::SecureDir;
use serde::{Deserialize, Serialize};

use crate::error::MemoryError;
use crate::storage::{legacy_storage_component, storage_key};

/// Dimensionality of the hashed fallback embedding.
pub const EMBED_DIM: usize = 512;

/// A hit from a semantic search, ordered most-relevant first.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemorySearchResult {
    pub text: String,
    /// Cosine similarity in [-1, 1] — higher is more relevant.
    pub score: f32,
    pub metadata: serde_json::Value,
}

/// One stored memory: the text, its embedding, and arbitrary metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct MemoryRecord {
    id: String,
    text: String,
    vector: Vec<f32>,
    metadata: serde_json::Value,
    ts: i64,
}

/// On-disk shape of the store — records plus the embedder that produced them.
#[derive(Debug, Serialize, Deserialize)]
struct StoredState {
    embedder: String,
    records: Vec<MemoryRecord>,
}

/// The embedding backend in use for a store.
enum Embedder {
    /// Neural sentence embeddings (`all-MiniLM-L6-v2` via Candle).
    #[cfg(feature = "neural-embeddings")]
    Neural(std::sync::Arc<crate::neural::NeuralEmbedder>),
    /// Pure-Rust lexical fallback (signed feature hashing).
    Hashed,
}

impl Embedder {
    /// Stable identifier — recorded in the store; a change triggers a re-embed.
    fn id(&self) -> &'static str {
        match self {
            #[cfg(feature = "neural-embeddings")]
            Embedder::Neural(_) => crate::neural::NEURAL_ID,
            Embedder::Hashed => "hashed-v1",
        }
    }

    /// Output dimensionality.
    fn dim(&self) -> usize {
        match self {
            #[cfg(feature = "neural-embeddings")]
            Embedder::Neural(_) => crate::neural::NEURAL_DIM,
            Embedder::Hashed => EMBED_DIM,
        }
    }

    /// Embed `text`. Infallible: a neural failure falls back to a zero vector
    /// of the correct dimension (it simply won't match anything) rather than
    /// poisoning the store with a wrong-dimension vector.
    fn embed(&self, text: &str) -> Vec<f32> {
        match self {
            #[cfg(feature = "neural-embeddings")]
            Embedder::Neural(n) => n.embed(text).unwrap_or_else(|e| {
                tracing::warn!(error = %e, "neural embed failed for this turn");
                vec![0.0; crate::neural::NEURAL_DIM]
            }),
            Embedder::Hashed => embed_hashed(text),
        }
    }
}

/// Choose the best available embedder: the neural model if it loads, otherwise
/// the lexical fallback.
fn pick_embedder() -> Embedder {
    #[cfg(feature = "neural-embeddings")]
    {
        match crate::neural::NeuralEmbedder::shared() {
            Ok(n) => return Embedder::Neural(n),
            Err(e) => tracing::warn!(
                error = %e,
                "neural embedder unavailable — using the lexical fallback"
            ),
        }
    }
    Embedder::Hashed
}

fn pick_embedder_in(data_root: &SecureDir) -> Embedder {
    #[cfg(feature = "neural-embeddings")]
    {
        match crate::neural::NeuralEmbedder::shared_in(data_root) {
            Ok(n) => return Embedder::Neural(n),
            Err(e) => tracing::warn!(
                error = %e,
                "neural embedder unavailable — using the lexical fallback"
            ),
        }
    }
    Embedder::Hashed
}

/// Tier 4 semantic memory for one agent (or one `{session}:{agent}`).
pub struct SemanticMemory {
    secure_dir: SecureDir,
    file_name: OsString,
    embedder: Embedder,
    records: Mutex<Vec<MemoryRecord>>,
}

impl SemanticMemory {
    /// Open (or create) the semantic store for `agent_id` under `dir`, using
    /// the best available embedder. The first call may download the neural
    /// model (~90 MB, cached thereafter).
    pub fn new(agent_id: &str, dir: impl Into<PathBuf>) -> Result<Self, MemoryError> {
        Self::with_embedder(agent_id, dir, pick_embedder())
    }

    /// Open a store that always uses the lexical fallback embedder — no model
    /// download. Intended for tests and offline/lean builds.
    pub fn new_hashed(agent_id: &str, dir: impl Into<PathBuf>) -> Result<Self, MemoryError> {
        Self::with_embedder(agent_id, dir, Embedder::Hashed)
    }

    /// Open a semantic store relative to the control-plane data root.
    pub fn new_in(
        agent_id: &str,
        data_root: impl AsRef<Path>,
        relative: impl AsRef<Path>,
    ) -> Result<Self, MemoryError> {
        let data_root = SecureDir::open(data_root)?;
        Self::new_in_secure(agent_id, &data_root, relative)
    }

    pub fn new_in_secure(
        agent_id: &str,
        data_root: &SecureDir,
        relative: impl AsRef<Path>,
    ) -> Result<Self, MemoryError> {
        let dir = data_root.child(relative)?;
        Self::with_embedder_in(agent_id, dir, pick_embedder_in(data_root))
    }

    fn with_embedder(
        agent_id: &str,
        dir: impl Into<PathBuf>,
        embedder: Embedder,
    ) -> Result<Self, MemoryError> {
        let dir = dir.into();
        let secure_dir = SecureDir::open_or_create_all(&dir)?;
        Self::with_embedder_in(agent_id, secure_dir, embedder)
    }

    fn with_embedder_in(
        agent_id: &str,
        secure_legacy_dir: SecureDir,
        embedder: Embedder,
    ) -> Result<Self, MemoryError> {
        let secure_dir = secure_legacy_dir.child("v1")?;
        let file_name = OsString::from(format!("agent_{}_semantic.json", storage_key(agent_id)));
        let legacy_path = legacy_storage_component(agent_id)
            .map(|component| OsString::from(format!("agent_{component}_semantic.json")));
        let current_exists = secure_dir.is_file(&file_name)?;
        let legacy_exists = match legacy_path.as_ref() {
            Some(legacy) => secure_legacy_dir.has_exact_file(legacy)?,
            None => false,
        };
        let reading_legacy = !current_exists && legacy_exists;
        let read_name = if current_exists {
            Some(&file_name)
        } else if legacy_exists {
            legacy_path.as_ref()
        } else {
            None
        };

        let records = if let Some(read_name) = read_name {
            let bytes = if current_exists {
                secure_dir.read(read_name)?
            } else {
                secure_legacy_dir.read(read_name)?
            };
            match serde_json::from_slice::<StoredState>(&bytes) {
                Ok(mut state) => {
                    // The embedder changed since this store was written —
                    // re-embed every memory so all vectors share one space.
                    if state.embedder != embedder.id() {
                        tracing::info!(
                            from = %state.embedder,
                            to = %embedder.id(),
                            count = state.records.len(),
                            "re-embedding semantic memory for the new model"
                        );
                        for r in &mut state.records {
                            r.vector = embedder.embed(&r.text);
                        }
                    }
                    state.records
                }
                // Do not let a failed compatibility read create an empty
                // current file that permanently hides the only legacy source.
                Err(error) if reading_legacy => return Err(error.into()),
                // Unreadable current format — start fresh rather than fail.
                Err(e) => {
                    tracing::warn!(error = %e, "semantic store unreadable — starting fresh");
                    Vec::new()
                }
            }
        } else {
            Vec::new()
        };

        let mem = Self {
            secure_dir,
            file_name,
            embedder,
            records: Mutex::new(records),
        };
        // Persist if a re-embed happened, so the migration is done once.
        mem.persist()?;
        Ok(mem)
    }

    /// Embed a string with the active backend.
    pub fn embed(&self, text: &str) -> Result<Vec<f32>, MemoryError> {
        Ok(self.embedder.embed(text))
    }

    /// Store a memory: embed `text`, append it with `metadata`, persist.
    /// Returns the new record id.
    pub fn store(&self, text: &str, metadata: serde_json::Value) -> Result<String, MemoryError> {
        let vector = self.embedder.embed(text);
        let id = format!(
            "mem-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        {
            let mut records = self.lock_records()?;
            records.push(MemoryRecord {
                id: id.clone(),
                text: text.to_string(),
                vector,
                metadata,
                ts: now_secs(),
            });
        }
        self.persist()?;
        Ok(id)
    }

    /// Return the `k` memories most semantically similar to `query`.
    /// A cold/empty store returns an empty list — never an error.
    pub fn search(&self, query: &str, k: usize) -> Result<Vec<MemorySearchResult>, MemoryError> {
        let records = self.lock_records()?;
        if records.is_empty() || k == 0 {
            return Ok(Vec::new());
        }
        let qv = self.embedder.embed(query);
        let mut scored: Vec<(f32, &MemoryRecord)> = records
            .iter()
            .map(|r| (cosine_similarity(&qv, &r.vector), r))
            .collect();
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        Ok(scored
            .into_iter()
            .take(k)
            .map(|(score, r)| MemorySearchResult {
                text: r.text.clone(),
                score,
                metadata: r.metadata.clone(),
            })
            .collect())
    }

    /// Up to `n` most-recently-stored memories (their text), oldest-first.
    /// Used by background consolidation to see "what's new" — a read-only feed
    /// (the semantic store is never evicted). Empty store → empty list.
    pub fn recent(&self, n: usize) -> Result<Vec<String>, MemoryError> {
        let records = self.lock_records()?;
        if records.is_empty() || n == 0 {
            return Ok(Vec::new());
        }
        let mut refs: Vec<&MemoryRecord> = records.iter().collect();
        refs.sort_by_key(|r| r.ts);
        let start = refs.len().saturating_sub(n);
        Ok(refs[start..].iter().map(|r| r.text.clone()).collect())
    }

    /// Embedding dimensionality of the active backend.
    pub fn dimensions(&self) -> usize {
        self.embedder.dim()
    }

    /// Number of stored memories.
    pub fn len(&self) -> usize {
        self.records.lock().map(|r| r.len()).unwrap_or(0)
    }

    /// True iff no memories are stored.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn lock_records(&self) -> Result<std::sync::MutexGuard<'_, Vec<MemoryRecord>>, MemoryError> {
        self.records
            .lock()
            .map_err(|e| MemoryError::VectorDb(e.to_string()))
    }

    /// Atomically write the store to disk (temp file + rename).
    fn persist(&self) -> Result<(), MemoryError> {
        let records = self.lock_records()?.clone();
        let state = StoredState {
            embedder: self.embedder.id().to_string(),
            records,
        };
        let bytes = serde_json::to_vec(&state).map_err(|e| MemoryError::VectorDb(e.to_string()))?;
        self.secure_dir.atomic_write(&self.file_name, &bytes)?;
        Ok(())
    }
}

/// FNV-1a hash — small, fast, deterministic, no dependency.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Lexical fallback embedding — signed feature hashing over words + character
/// trigrams, L2-normalised.
fn embed_hashed(text: &str) -> Vec<f32> {
    let mut v = vec![0.0f32; EMBED_DIM];
    let lower = text.to_lowercase();

    let mut add_token = |tok: &str| {
        if tok.is_empty() {
            return;
        }
        let h = fnv1a(tok.as_bytes());
        let idx = (h % EMBED_DIM as u64) as usize;
        let sign = if (h >> 63) & 1 == 1 { 1.0 } else { -1.0 };
        v[idx] += sign;
    };

    for word in lower.split(|c: char| !c.is_alphanumeric()) {
        if word.is_empty() {
            continue;
        }
        add_token(word);
        let chars: Vec<char> = word.chars().collect();
        if chars.len() >= 3 {
            for w in chars.windows(3) {
                let tri: String = w.iter().collect();
                add_token(&tri);
            }
        }
    }

    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in &mut v {
            *x /= norm;
        }
    }
    v
}

/// Cosine similarity of two equal-length vectors. Returns 0 for a
/// zero-magnitude or mismatched-length input rather than NaN.
fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for i in 0..a.len() {
        dot += a[i] * b[i];
        na += a[i] * a[i];
        nb += b[i] * b[i];
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn cosine_basics() {
        assert!((cosine_similarity(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-6);
        assert!(cosine_similarity(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-6);
        assert_eq!(cosine_similarity(&[1.0], &[1.0, 2.0]), 0.0);
        assert_eq!(cosine_similarity(&[0.0, 0.0], &[1.0, 1.0]), 0.0);
    }

    #[test]
    fn hashed_embedding_is_normalised_and_lexically_sensible() {
        let a = embed_hashed("the user prefers dark mode in the editor");
        let norm: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-4, "embedding must be L2-normalised");

        let related = cosine_similarity(&a, &embed_hashed("user likes a dark editor theme"));
        let unrelated = cosine_similarity(&a, &embed_hashed("schedule a flight to Tokyo"));
        assert!(
            related > unrelated,
            "lexically-related text must score higher"
        );
    }

    #[test]
    fn store_search_and_persistence_roundtrip() {
        let dir = tempdir().unwrap();
        {
            // `new_hashed` keeps the test offline + fast (no model download).
            let mem = SemanticMemory::new_hashed("a1", dir.path()).unwrap();
            assert!(mem.is_empty());
            mem.store(
                "the deploy script lives in scripts/release.sh",
                serde_json::json!({}),
            )
            .unwrap();
            mem.store("the user's name is Ada", serde_json::json!({"k": "name"}))
                .unwrap();
            let hits = mem.search("where is the deploy script", 1).unwrap();
            assert_eq!(hits.len(), 1);
            assert!(hits[0].text.contains("release.sh"));
        }
        // Reopen — records survive.
        let mem = SemanticMemory::new_hashed("a1", dir.path()).unwrap();
        assert_eq!(mem.len(), 2);
        assert_eq!(mem.search("nothing", 0).unwrap().len(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn scoped_semantic_store_promotes_safe_legacy_file() {
        let dir = tempdir().unwrap();
        let id = "ses-123:coder";
        let mem = SemanticMemory::new_hashed(id, dir.path()).unwrap();
        mem.store("legacy semantic fact", serde_json::json!({}))
            .unwrap();
        drop(mem);
        let current = dir
            .path()
            .join("v1")
            .join(format!("agent_{}_semantic.json", storage_key(id)));
        let legacy = dir.path().join(format!("agent_{id}_semantic.json"));
        std::fs::rename(&current, &legacy).unwrap();

        let reopened = SemanticMemory::new_hashed(id, dir.path()).unwrap();
        assert_eq!(reopened.len(), 1);
        assert!(current.is_file());
        assert!(legacy.is_file());
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_legacy_semantic_does_not_create_a_hiding_current_file() {
        let dir = tempdir().unwrap();
        let id = "ses-123:coder";
        let legacy = dir.path().join(format!("agent_{id}_semantic.json"));
        std::fs::write(&legacy, b"not json").unwrap();
        let current = dir
            .path()
            .join("v1")
            .join(format!("agent_{}_semantic.json", storage_key(id)));

        assert!(SemanticMemory::new_hashed(id, dir.path()).is_err());
        assert!(!current.exists());
        assert!(legacy.is_file());
    }

    #[test]
    fn traversal_id_cannot_escape_semantic_directory() {
        let parent = tempdir().unwrap();
        let dir = parent.path().join("semantic");
        let outside = parent.path().join("outside_semantic.json");
        std::fs::write(&outside, b"sentinel").unwrap();

        let mem = SemanticMemory::new_hashed("../outside", &dir).unwrap();
        mem.store("contained", serde_json::json!({})).unwrap();
        let current = dir
            .join("v1")
            .join(format!("agent_{}_semantic.json", storage_key("../outside")));
        assert!(current.is_file());
        assert_eq!(std::fs::read(outside).unwrap(), b"sentinel");
    }

    #[cfg(unix)]
    #[test]
    fn lowercase_semantic_store_does_not_adopt_uppercase_legacy_file() {
        let dir = tempdir().unwrap();
        let uppercase = SemanticMemory::new_hashed("Coder", dir.path()).unwrap();
        uppercase
            .store("uppercase legacy fact", serde_json::json!({}))
            .unwrap();
        drop(uppercase);
        let uppercase_current = dir
            .path()
            .join("v1")
            .join(format!("agent_{}_semantic.json", storage_key("Coder")));
        let uppercase_legacy = dir.path().join("agent_Coder_semantic.json");
        std::fs::rename(&uppercase_current, &uppercase_legacy).unwrap();

        let lowercase = SemanticMemory::new_hashed("coder", dir.path()).unwrap();
        assert!(lowercase.is_empty());
        lowercase
            .store("lowercase current fact", serde_json::json!({}))
            .unwrap();
        assert_eq!(lowercase.len(), 1);

        let reopened_uppercase = SemanticMemory::new_hashed("Coder", dir.path()).unwrap();
        assert_eq!(reopened_uppercase.len(), 1);
        assert_eq!(
            reopened_uppercase.recent(1).unwrap(),
            vec!["uppercase legacy fact"]
        );
        assert!(dir.path().join("v1/agent_coder_semantic.json").is_file());
        assert!(uppercase_legacy.is_file());
    }
}
