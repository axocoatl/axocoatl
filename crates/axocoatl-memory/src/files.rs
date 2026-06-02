//! Local content-addressed file store — the closest thing to a "Files API"
//! we can have while staying local-first.
//!
//! ## Design
//!
//! Every uploaded file is hashed (SHA-256) and stored at
//! `{root}/{aa}/{full_hash}.{ext}` where `aa` is the first two hex chars.
//! Same bytes uploaded twice = one copy on disk (dedup is free).
//!
//! Each file has a sidecar `{root}/{aa}/{full_hash}.meta.json` carrying:
//! - the original filename + MIME the user uploaded with
//! - extracted text (PDF, CSV, XLSX → pure text for LLM consumption)
//! - OCR text (image → tesseract output, if tesseract is on PATH)
//! - tags + a renameable display label
//!
//! ## Why content-addressed?
//!
//! Three wins:
//! 1. **Dedup** — drop the same PDF onto two different chats, one disk copy.
//! 2. **Stable ids** — the id IS the content. A chat that pins a file
//!    survives renames of the original; a file with the same content
//!    re-uploaded gets the same id.
//! 3. **No vendor lock-in** — the id space is universal (SHA-256), not
//!    coupled to any provider's Files API.

use crate::error::MemoryError;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// One file in the store. `id` is the SHA-256 of the bytes (hex).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntry {
    /// Content hash — also the on-disk filename root.
    pub id: String,
    /// User-facing label (defaults to original filename, editable via `rename`).
    pub name: String,
    /// MIME type at upload time.
    pub mime: String,
    /// File extension (no leading dot).
    pub ext: String,
    /// Size in bytes.
    pub size: u64,
    /// Unix-seconds upload time.
    pub uploaded_at: u64,
    /// Text extracted from the file at store-time (PDF / CSV / XLSX / TXT).
    /// `None` if extraction wasn't applicable or failed.
    #[serde(default)]
    pub extracted_text: Option<String>,
    /// OCR output for images (Tesseract). `None` if the binary isn't installed
    /// or the image yielded no text.
    #[serde(default)]
    pub ocr_text: Option<String>,
    /// Free-form tags for the user's organization.
    #[serde(default)]
    pub tags: Vec<String>,
}

impl FileEntry {
    pub fn is_image(&self) -> bool {
        self.mime.starts_with("image/")
    }
    /// Best textual representation for inlining into an LLM prompt: prefers
    /// extracted_text (PDF/CSV/XLSX), falls back to OCR for images.
    pub fn inline_text(&self) -> Option<&str> {
        self.extracted_text.as_deref().or(self.ocr_text.as_deref())
    }
}

/// JSON-on-disk file store. One sidecar `*.meta.json` per stored file plus
/// the bytes themselves. The in-memory `entries` map mirrors what's on disk;
/// rebuild by calling [`FileStore::load_all`].
pub struct FileStore {
    root: PathBuf,
    entries: HashMap<String, FileEntry>,
}

impl FileStore {
    /// Open (creating if absent) the store rooted at `root`.
    pub fn new(root: impl Into<PathBuf>) -> Result<Self, MemoryError> {
        let root = root.into();
        std::fs::create_dir_all(&root)?;
        Ok(Self {
            root,
            entries: HashMap::new(),
        })
    }

    /// Crawl the on-disk tree and load every sidecar into memory.
    /// Malformed sidecars are skipped (logged by the caller), not fatal.
    pub fn load_all(&mut self) -> Result<(), MemoryError> {
        if !self.root.exists() {
            return Ok(());
        }
        for shard in std::fs::read_dir(&self.root)? {
            let shard = shard?;
            if !shard.file_type()?.is_dir() {
                continue;
            }
            for entry in std::fs::read_dir(shard.path())? {
                let path = entry?.path();
                let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                    continue;
                };
                if !name.ends_with(".meta.json") {
                    continue;
                }
                if let Ok(bytes) = std::fs::read(&path) {
                    if let Ok(entry) = serde_json::from_slice::<FileEntry>(&bytes) {
                        self.entries.insert(entry.id.clone(), entry);
                    }
                }
            }
        }
        Ok(())
    }

    /// Store bytes under their content hash. If the same bytes are already
    /// stored, returns the existing entry without rewriting. The extractor
    /// closure runs only on a fresh store — it gets `(bytes, mime)` and
    /// returns `(extracted_text, ocr_text)`.
    ///
    /// The split on hash dedup means a user can drop the same 50-page PDF
    /// onto five chats and only pay extraction cost once.
    pub fn store_with<F>(
        &mut self,
        bytes: &[u8],
        original_name: &str,
        mime: &str,
        extractor: F,
    ) -> Result<FileEntry, MemoryError>
    where
        F: FnOnce(&[u8], &str) -> (Option<String>, Option<String>),
    {
        let id = sha256_hex(bytes);
        if let Some(existing) = self.entries.get(&id) {
            return Ok(existing.clone());
        }
        let ext = Path::new(original_name)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("bin")
            .to_lowercase();
        let shard = self.shard_dir(&id);
        std::fs::create_dir_all(&shard)?;
        let path = shard.join(format!("{id}.{ext}"));
        std::fs::write(&path, bytes)?;
        let (extracted_text, ocr_text) = extractor(bytes, mime);
        let entry = FileEntry {
            id: id.clone(),
            name: original_name.to_string(),
            mime: mime.to_string(),
            ext,
            size: bytes.len() as u64,
            uploaded_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            extracted_text,
            ocr_text,
            tags: Vec::new(),
        };
        self.persist(&entry)?;
        self.entries.insert(id, entry.clone());
        Ok(entry)
    }

    /// Look up an entry by id (content hash).
    pub fn get(&self, id: &str) -> Option<FileEntry> {
        self.entries.get(id).cloned()
    }

    /// Read the raw bytes from disk. Use sparingly — callers should usually
    /// hand the path off rather than slurping the whole file into memory.
    pub fn read_bytes(&self, id: &str) -> Result<Vec<u8>, MemoryError> {
        let entry = self
            .entries
            .get(id)
            .ok_or_else(|| MemoryError::NotFound(format!("file {id} not found")))?;
        let path = self.shard_dir(id).join(format!("{}.{}", id, entry.ext));
        Ok(std::fs::read(path)?)
    }

    /// Absolute path to the file on disk. Used by callers (e.g. the chat
    /// executor) that prefer to hand a path off to downstream tooling rather
    /// than slurping the bytes themselves.
    pub fn path_of(&self, id: &str) -> Option<PathBuf> {
        let entry = self.entries.get(id)?;
        Some(self.shard_dir(id).join(format!("{}.{}", id, entry.ext)))
    }

    /// All files, newest first.
    pub fn list(&self) -> Vec<FileEntry> {
        let mut v: Vec<FileEntry> = self.entries.values().cloned().collect();
        v.sort_by_key(|x| std::cmp::Reverse(x.uploaded_at));
        v
    }

    /// Case-insensitive substring search across name, tags, extracted_text,
    /// and ocr_text. Empty query = full list.
    pub fn search(&self, query: &str) -> Vec<FileEntry> {
        let q = query.trim().to_lowercase();
        if q.is_empty() {
            return self.list();
        }
        let mut hits: Vec<FileEntry> = self
            .entries
            .values()
            .filter(|f| {
                f.name.to_lowercase().contains(&q)
                    || f.tags.iter().any(|t| t.to_lowercase().contains(&q))
                    || f.extracted_text
                        .as_deref()
                        .map(|t| t.to_lowercase().contains(&q))
                        .unwrap_or(false)
                    || f.ocr_text
                        .as_deref()
                        .map(|t| t.to_lowercase().contains(&q))
                        .unwrap_or(false)
            })
            .cloned()
            .collect();
        hits.sort_by_key(|x| std::cmp::Reverse(x.uploaded_at));
        hits
    }

    /// Rename the user-facing label (the file id stays content-derived).
    pub fn rename(&mut self, id: &str, new_name: &str) -> Result<FileEntry, MemoryError> {
        let name = new_name.trim();
        if name.is_empty() {
            return Err(MemoryError::Invalid("name is empty".to_string()));
        }
        let entry = self
            .entries
            .get_mut(id)
            .ok_or_else(|| MemoryError::NotFound(format!("file {id} not found")))?;
        entry.name = name.to_string();
        let snap = entry.clone();
        self.persist(&snap)?;
        Ok(snap)
    }

    /// Replace the tag list.
    pub fn set_tags(&mut self, id: &str, tags: Vec<String>) -> Result<FileEntry, MemoryError> {
        let entry = self
            .entries
            .get_mut(id)
            .ok_or_else(|| MemoryError::NotFound(format!("file {id} not found")))?;
        entry.tags = tags;
        let snap = entry.clone();
        self.persist(&snap)?;
        Ok(snap)
    }

    /// Delete the file from disk and the in-memory index. Callers should
    /// also clean up any chat references (the store doesn't know about chats).
    pub fn remove(&mut self, id: &str) -> Result<(), MemoryError> {
        let Some(entry) = self.entries.remove(id) else {
            return Err(MemoryError::NotFound(format!("file {id} not found")));
        };
        let shard = self.shard_dir(id);
        let _ = std::fs::remove_file(shard.join(format!("{}.{}", id, entry.ext)));
        let _ = std::fs::remove_file(shard.join(format!("{id}.meta.json")));
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn shard_dir(&self, id: &str) -> PathBuf {
        let prefix = &id[..2.min(id.len())];
        self.root.join(prefix)
    }

    fn persist(&self, entry: &FileEntry) -> Result<(), MemoryError> {
        let shard = self.shard_dir(&entry.id);
        std::fs::create_dir_all(&shard)?;
        let path = shard.join(format!("{}.meta.json", entry.id));
        let tmp = path.with_extension("json.tmp");
        let bytes = serde_json::to_vec_pretty(entry)?;
        std::fs::write(&tmp, &bytes)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    let result = h.finalize();
    let mut s = String::with_capacity(result.len() * 2);
    for byte in result {
        use std::fmt::Write;
        let _ = write!(&mut s, "{byte:02x}");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn store_dedups_identical_bytes() {
        let dir = tempdir().unwrap();
        let mut s = FileStore::new(dir.path().join("files")).unwrap();
        let a = s
            .store_with(b"hello", "a.txt", "text/plain", |_, _| (None, None))
            .unwrap();
        let b = s
            .store_with(b"hello", "b.txt", "text/plain", |_, _| (None, None))
            .unwrap();
        // Same bytes → same id → one entry. The second store doesn't overwrite
        // the metadata (the first upload's name wins).
        assert_eq!(a.id, b.id);
        assert_eq!(s.len(), 1);
        assert_eq!(b.name, "a.txt");
    }

    #[test]
    fn extractor_runs_on_first_store_only() {
        let dir = tempdir().unwrap();
        let mut s = FileStore::new(dir.path().join("files")).unwrap();
        let calls = std::cell::Cell::new(0);
        let _ = s
            .store_with(b"x", "f.txt", "text/plain", |_, _| {
                calls.set(calls.get() + 1);
                (Some("extracted".into()), None)
            })
            .unwrap();
        let _ = s
            .store_with(b"x", "f.txt", "text/plain", |_, _| {
                calls.set(calls.get() + 1);
                (Some("would re-extract".into()), None)
            })
            .unwrap();
        assert_eq!(calls.get(), 1);
        let e = s.list().pop().unwrap();
        assert_eq!(e.extracted_text.as_deref(), Some("extracted"));
    }

    #[test]
    fn load_all_roundtrips() {
        let dir = tempdir().unwrap();
        let mut s = FileStore::new(dir.path().join("files")).unwrap();
        let entry = s
            .store_with(b"persistent", "doc.txt", "text/plain", |_, _| {
                (Some("hi".into()), None)
            })
            .unwrap();
        let mut reopen = FileStore::new(dir.path().join("files")).unwrap();
        reopen.load_all().unwrap();
        assert_eq!(reopen.len(), 1);
        let loaded = reopen.get(&entry.id).unwrap();
        assert_eq!(loaded.name, "doc.txt");
        assert_eq!(loaded.extracted_text.as_deref(), Some("hi"));
    }

    #[test]
    fn search_finds_in_extracted_text() {
        let dir = tempdir().unwrap();
        let mut s = FileStore::new(dir.path().join("files")).unwrap();
        s.store_with(b"unique", "a.txt", "text/plain", |_, _| {
            (Some("axocoatl is a feathered serpent".into()), None)
        })
        .unwrap();
        let hits = s.search("feathered");
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn remove_clears_disk_and_index() {
        let dir = tempdir().unwrap();
        let mut s = FileStore::new(dir.path().join("files")).unwrap();
        let e = s
            .store_with(b"goodbye", "g.txt", "text/plain", |_, _| (None, None))
            .unwrap();
        let p = s.path_of(&e.id).unwrap();
        assert!(p.exists());
        s.remove(&e.id).unwrap();
        assert!(!p.exists());
        assert_eq!(s.len(), 0);
    }
}
