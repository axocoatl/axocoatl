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
use crate::extract::{ExtractionLimits, ExtractionOutput};
use axocoatl_core::{SecureDir, SecureEntryType};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Immutable, content-derived metadata that a session-owned reference can
/// safely snapshot without inheriting the compatibility API's mutable label
/// and tags.
///
/// The media type and filename deliberately do not live here. Both are
/// uploader-supplied presentation metadata rather than properties proven by
/// the blob bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobMetadata {
    /// SHA-256 of the exact stored bytes.
    pub id: String,
    /// Size of the exact stored bytes.
    pub size: u64,
    /// Unix-seconds time at which this blob first entered the store.
    pub stored_at: u64,
    /// Cached, bounded extraction details.
    #[serde(default)]
    pub extraction: ExtractionMetadata,
}

/// Uploader-supplied presentation metadata. A future session reference should
/// own a copy of this structure rather than mutating the global blob record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FilePresentation {
    pub display_name: String,
    /// Declared by the uploader and therefore untrusted; this is not a MIME
    /// type inferred from the content.
    pub declared_media_type: String,
    /// Sanitized extension used by the compatibility store's physical path.
    pub extension: String,
    #[serde(default)]
    pub tags: Vec<String>,
}

impl FilePresentation {
    /// Capture presentation metadata from this particular upload. Session
    /// references should use this constructor rather than copying a deduped
    /// `FileEntry`, whose compatibility label belongs to the first upload.
    pub fn from_upload(original_name: &str, declared_media_type: &str) -> Self {
        let display_name = Path::new(original_name)
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .unwrap_or("attachment")
            .to_string();
        let extension = sanitize_extension(
            Path::new(&display_name)
                .extension()
                .and_then(|extension| extension.to_str())
                .unwrap_or("bin"),
        );
        Self {
            display_name,
            declared_media_type: declared_media_type.to_string(),
            extension,
            tags: Vec::new(),
        }
    }
}

/// Metadata about one bounded text representation cached beside a blob.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextExtractionMetadata {
    /// Whether bytes were omitted to keep the cached representation bounded.
    #[serde(default)]
    pub truncated: bool,
    /// UTF-8 byte length of the representation stored in the sidecar.
    #[serde(default)]
    pub stored_bytes: u64,
    /// Bytes observed before the representation was bounded. A streaming
    /// extractor may report only `limit + 1`, which proves truncation without
    /// materializing the complete output.
    #[serde(default)]
    pub source_bytes: Option<u64>,
}

/// Versioned extraction metadata. Version zero denotes a sidecar written by
/// an older Axocoatl build, before bounded extraction metadata was recorded.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtractionMetadata {
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub status: ExtractionStatus,
    #[serde(default)]
    pub extracted_text: Option<TextExtractionMetadata>,
    #[serde(default)]
    pub ocr_text: Option<TextExtractionMetadata>,
}

/// Availability of the cached representation, separate from the text itself.
/// `Unknown` is reserved for legacy sidecars and compatibility extractors that
/// could not report whether extraction was applicable.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtractionStatus {
    #[default]
    Unknown,
    NotApplicable,
    Complete,
    Unavailable,
}

/// Explicit ingestion bound for APIs that receive a reader. There is no
/// implicit product-wide upload limit here because image, document, and future
/// media policies differ; callers must choose the limit for their seam.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlobIngestLimit {
    pub max_bytes: u64,
}

impl BlobIngestLimit {
    pub const fn new(max_bytes: u64) -> Self {
        Self { max_bytes }
    }
}

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
    /// Versioned size/truncation facts for the cached textual representations.
    /// Older sidecars deserialize with version zero.
    #[serde(default)]
    pub extraction: ExtractionMetadata,
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

    pub fn blob_metadata(&self) -> BlobMetadata {
        BlobMetadata {
            id: self.id.clone(),
            size: self.size,
            stored_at: self.uploaded_at,
            extraction: self.extraction.clone(),
        }
    }

    pub fn presentation(&self) -> FilePresentation {
        FilePresentation {
            display_name: self.name.clone(),
            declared_media_type: self.mime.clone(),
            extension: self.ext.clone(),
            tags: self.tags.clone(),
        }
    }
}

/// JSON-on-disk file store. One sidecar `*.meta.json` per stored file plus
/// the bytes themselves. The in-memory `entries` map mirrors what's on disk;
/// rebuild by calling [`FileStore::load_all`].
pub struct FileStore {
    secure_root: SecureDir,
    entries: HashMap<String, FileEntry>,
}

impl FileStore {
    /// Open (creating if absent) the store rooted at `root`.
    pub fn new(root: impl Into<PathBuf>) -> Result<Self, MemoryError> {
        let root = root.into();
        let secure_root = SecureDir::open_or_create_all(&root)?;
        Ok(Self {
            secure_root,
            entries: HashMap::new(),
        })
    }

    pub fn new_in(
        data_root: impl AsRef<Path>,
        relative: impl AsRef<Path>,
    ) -> Result<Self, MemoryError> {
        let data_root = SecureDir::open(data_root)?;
        Self::new_in_secure(&data_root, relative)
    }

    pub fn new_in_secure(
        data_root: &SecureDir,
        relative: impl AsRef<Path>,
    ) -> Result<Self, MemoryError> {
        let secure_root = data_root.child(relative)?;
        Ok(Self {
            secure_root,
            entries: HashMap::new(),
        })
    }

    /// Crawl the on-disk tree and load every sidecar into memory.
    /// Malformed sidecars are skipped (logged by the caller), not fatal.
    pub fn load_all(&mut self) -> Result<(), MemoryError> {
        for shard in self.secure_root.entries()? {
            if shard.file_type != SecureEntryType::Directory {
                continue;
            }
            let shard_dir = self.secure_root.existing_child(&shard.name)?;
            for entry in shard_dir.entries()? {
                if entry.file_type != SecureEntryType::File {
                    continue;
                }
                let Some(name) = entry.name.to_str() else {
                    continue;
                };
                if !name.ends_with(".meta.json") {
                    continue;
                }
                if let Ok(bytes) = shard_dir.read(&entry.name) {
                    if let Ok(mut entry) = serde_json::from_slice::<FileEntry>(&bytes) {
                        // Sidecars are local data, but still validate anything
                        // later used to construct an anchored relative path.
                        // This prevents traversal and ensures the sidecar name
                        // agrees with the claimed content id.
                        let expected_name = format!("{}.meta.json", entry.id);
                        if !is_sha256_hex(&entry.id)
                            || name != expected_name
                            || sanitize_extension(&entry.ext) != entry.ext
                        {
                            continue;
                        }
                        // Fill truthful byte counts for legacy sidecars where
                        // possible without claiming that older extraction was
                        // complete.
                        hydrate_legacy_extraction_metadata(&mut entry);
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
        self.store_with_output(bytes, original_name, mime, |bytes, mime| {
            ExtractionOutput::from_legacy(extractor(bytes, mime))
        })
    }

    /// Store bytes with versioned, bounded extraction metadata. This is the
    /// preferred API for new session context references. Existing ChatStore
    /// callers can continue using [`FileStore::store_with`].
    pub fn store_with_output<F>(
        &mut self,
        bytes: &[u8],
        original_name: &str,
        mime: &str,
        extractor: F,
    ) -> Result<FileEntry, MemoryError>
    where
        F: FnOnce(&[u8], &str) -> ExtractionOutput,
    {
        let id = sha256_hex(bytes);
        if let Some(existing) = self.entries.get(&id) {
            return Ok(existing.clone());
        }
        let ext = sanitize_extension(
            Path::new(original_name)
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("bin"),
        );
        let relative = self.blob_relative(&id, &ext);
        self.secure_root.atomic_write(&relative, bytes)?;
        let output = extractor(bytes, mime).bounded(ExtractionLimits::default());
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
            extracted_text: output.extracted_text,
            ocr_text: output.ocr_text,
            extraction: output.metadata,
            tags: Vec::new(),
        };
        if let Err(error) = self.persist(&entry) {
            // A sidecar is the commit marker discovered by `load_all`. If it
            // cannot be written, leave no unindexed blob behind.
            let _ = self.secure_root.remove_file(&relative);
            return Err(error);
        }
        self.entries.insert(id, entry.clone());
        Ok(entry)
    }

    /// Store bytes only if they fit an explicit ingestion bound. The size is
    /// checked before hashing, writing, or extracting.
    pub fn store_bounded_with_output<F>(
        &mut self,
        bytes: &[u8],
        original_name: &str,
        mime: &str,
        limit: BlobIngestLimit,
        extractor: F,
    ) -> Result<FileEntry, MemoryError>
    where
        F: FnOnce(&[u8], &str) -> ExtractionOutput,
    {
        ensure_size_within(bytes.len() as u64, limit)?;
        self.store_with_output(bytes, original_name, mime, extractor)
    }

    /// Read at most `limit + 1` bytes before deciding whether to accept the
    /// blob. This prevents a stream with a false or absent length from being
    /// buffered without bound. `advertised_size` is an early rejection hint;
    /// the actual count remains authoritative.
    pub fn store_reader_with_output<R, F>(
        &mut self,
        mut reader: R,
        advertised_size: Option<u64>,
        original_name: &str,
        mime: &str,
        limit: BlobIngestLimit,
        extractor: F,
    ) -> Result<FileEntry, MemoryError>
    where
        R: Read,
        F: FnOnce(&[u8], &str) -> ExtractionOutput,
    {
        if let Some(size) = advertised_size {
            ensure_size_within(size, limit)?;
        }
        let read_cap = limit.max_bytes.saturating_add(1);
        let mut bytes = Vec::with_capacity(
            advertised_size
                .unwrap_or(0)
                .min(limit.max_bytes)
                .min(usize::MAX as u64) as usize,
        );
        reader.by_ref().take(read_cap).read_to_end(&mut bytes)?;
        ensure_size_within(bytes.len() as u64, limit)?;
        self.store_with_output(&bytes, original_name, mime, extractor)
    }

    /// Look up an entry by id (content hash).
    pub fn get(&self, id: &str) -> Option<FileEntry> {
        self.entries.get(id).cloned()
    }

    /// Read and authenticate the raw bytes through the retained store handle.
    ///
    /// A content-addressed id is an integrity contract, not only a filename.
    /// Refuse a blob whose bytes or length no longer match its sidecar rather
    /// than passing poisoned attachment content to an Agent.
    pub fn read_bytes(&self, id: &str) -> Result<Vec<u8>, MemoryError> {
        let entry = self
            .entries
            .get(id)
            .ok_or_else(|| MemoryError::NotFound(format!("file {id} not found")))?;
        let bytes = self.secure_root.read(self.blob_relative(id, &entry.ext))?;
        let actual_id = sha256_hex(&bytes);
        if bytes.len() as u64 != entry.size || actual_id != entry.id {
            return Err(MemoryError::Invalid(format!(
                "stored blob '{}' failed its content-addressed integrity check",
                entry.id
            )));
        }
        Ok(bytes)
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
        let mut snap = self
            .entries
            .get(id)
            .cloned()
            .ok_or_else(|| MemoryError::NotFound(format!("file {id} not found")))?;
        snap.name = name.to_string();
        self.persist(&snap)?;
        self.entries.insert(id.to_string(), snap.clone());
        Ok(snap)
    }

    /// Replace the tag list.
    pub fn set_tags(&mut self, id: &str, tags: Vec<String>) -> Result<FileEntry, MemoryError> {
        let mut snap = self
            .entries
            .get(id)
            .cloned()
            .ok_or_else(|| MemoryError::NotFound(format!("file {id} not found")))?;
        snap.tags = tags;
        self.persist(&snap)?;
        self.entries.insert(id.to_string(), snap.clone());
        Ok(snap)
    }

    /// Delete the sidecar commit marker, then the blob, then the in-memory
    /// entry. On error the entry remains indexed so the caller can retry.
    /// Callers should also clean up any chat references (the store doesn't
    /// know about chats).
    pub fn remove(&mut self, id: &str) -> Result<(), MemoryError> {
        let Some(entry) = self.entries.get(id).cloned() else {
            return Err(MemoryError::NotFound(format!("file {id} not found")));
        };
        let sidecar = self.sidecar_relative(id);
        let blob = self.blob_relative(id, &entry.ext);
        remove_secure_if_present(&self.secure_root, &sidecar)?;
        if let Err(delete_error) = remove_secure_if_present(&self.secure_root, &blob) {
            // Restore the sidecar commit marker so memory and restart state
            // continue to agree. If rollback itself fails, drop the in-memory
            // entry: the remaining raw blob is then an unindexed orphan, not
            // stale live metadata.
            if let Err(rollback_error) = self.persist(&entry) {
                self.entries.remove(id);
                return Err(MemoryError::Invalid(format!(
                    "blob delete failed ({delete_error}); sidecar rollback failed ({rollback_error})"
                )));
            }
            return Err(delete_error);
        }
        self.entries.remove(id);
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn sidecar_relative(&self, id: &str) -> PathBuf {
        PathBuf::from(&id[..2.min(id.len())]).join(format!("{id}.meta.json"))
    }

    fn blob_relative(&self, id: &str, extension: &str) -> PathBuf {
        PathBuf::from(&id[..2.min(id.len())]).join(format!("{id}.{extension}"))
    }

    fn persist(&self, entry: &FileEntry) -> Result<(), MemoryError> {
        let bytes = serde_json::to_vec_pretty(entry)?;
        self.secure_root
            .atomic_write(self.sidecar_relative(&entry.id), &bytes)
            .map_err(MemoryError::from)
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

fn remove_secure_if_present(root: &SecureDir, relative: &Path) -> Result<(), MemoryError> {
    if root.is_file(relative)? {
        root.remove_file(relative)?;
    }
    Ok(())
}

fn is_sha256_hex(id: &str) -> bool {
    id.len() == 64
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn sanitize_extension(ext: &str) -> String {
    let ext = ext.to_ascii_lowercase();
    if !ext.is_empty() && ext.len() <= 16 && ext.bytes().all(|b| b.is_ascii_alphanumeric()) {
        ext
    } else {
        "bin".to_string()
    }
}

fn ensure_size_within(size: u64, limit: BlobIngestLimit) -> Result<(), MemoryError> {
    if size > limit.max_bytes {
        return Err(MemoryError::BlobTooLarge {
            size,
            limit: limit.max_bytes,
        });
    }
    Ok(())
}

fn hydrate_legacy_extraction_metadata(entry: &mut FileEntry) {
    if entry.extraction.version != 0 {
        return;
    }
    entry.extraction.extracted_text =
        entry
            .extracted_text
            .as_ref()
            .map(|text| TextExtractionMetadata {
                truncated: false,
                stored_bytes: text.len() as u64,
                source_bytes: None,
            });
    entry.extraction.ocr_text = entry.ocr_text.as_ref().map(|text| TextExtractionMetadata {
        truncated: false,
        stored_bytes: text.len() as u64,
        source_bytes: None,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
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
    fn stored_blob_and_sidecar_leave_no_temp_files() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("files");
        let mut store = FileStore::new(&root).unwrap();
        let entry = store
            .store_with(b"atomic", "atomic.txt", "text/plain", |_, _| (None, None))
            .unwrap();
        let shard = root.join(&entry.id[..2]);
        let names: Vec<_> = std::fs::read_dir(&shard)
            .unwrap()
            .map(|item| item.unwrap().file_name())
            .collect();
        assert_eq!(names.len(), 2);
        assert!(names
            .iter()
            .all(|name| !name.to_string_lossy().ends_with(".tmp")));
        assert_eq!(store.read_bytes(&entry.id).unwrap(), b"atomic");
        assert!(shard.join(format!("{}.meta.json", entry.id)).exists());
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
        let p = dir
            .path()
            .join("files")
            .join(&e.id[..2])
            .join(format!("{}.{}", e.id, e.ext));
        assert_eq!(s.read_bytes(&e.id).unwrap(), b"goodbye");
        s.remove(&e.id).unwrap();
        assert!(!p.exists());
        assert_eq!(s.len(), 0);
    }

    #[test]
    fn remove_keeps_entry_indexed_when_disk_delete_fails() {
        let dir = tempdir().unwrap();
        let mut store = FileStore::new(dir.path().join("files")).unwrap();
        let entry = store
            .store_with(b"undeletable", "keep.txt", "text/plain", |_, _| {
                (None, None)
            })
            .unwrap();
        let sidecar = dir
            .path()
            .join("files")
            .join(&entry.id[..2])
            .join(format!("{}.meta.json", entry.id));
        std::fs::remove_file(&sidecar).unwrap();
        std::fs::create_dir(&sidecar).unwrap();

        assert!(store.remove(&entry.id).is_err());
        assert!(store.get(&entry.id).is_some());
        assert_eq!(store.read_bytes(&entry.id).unwrap(), b"undeletable");
    }

    #[test]
    fn reader_ingestion_rejects_actual_size_before_store_or_extract() {
        let dir = tempdir().unwrap();
        let mut store = FileStore::new(dir.path().join("files")).unwrap();
        let extracted = std::cell::Cell::new(false);
        let error = store
            .store_reader_with_output(
                Cursor::new(b"123456"),
                None,
                "oversized.txt",
                "text/plain",
                BlobIngestLimit::new(5),
                |_, _| {
                    extracted.set(true);
                    ExtractionOutput::default()
                },
            )
            .unwrap_err();
        assert!(matches!(
            error,
            MemoryError::BlobTooLarge { size: 6, limit: 5 }
        ));
        assert!(!extracted.get());
        assert!(store.is_empty());
    }

    #[test]
    fn reader_ingestion_rejects_advertised_size_without_reading() {
        struct MustNotRead;
        impl Read for MustNotRead {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                panic!("reader should not be touched after early size rejection")
            }
        }

        let dir = tempdir().unwrap();
        let mut store = FileStore::new(dir.path().join("files")).unwrap();
        let error = store
            .store_reader_with_output(
                MustNotRead,
                Some(6),
                "oversized.txt",
                "text/plain",
                BlobIngestLimit::new(5),
                |_, _| ExtractionOutput::default(),
            )
            .unwrap_err();
        assert!(matches!(
            error,
            MemoryError::BlobTooLarge { size: 6, limit: 5 }
        ));
    }

    #[test]
    fn immutable_metadata_is_separate_from_presentation() {
        let dir = tempdir().unwrap();
        let mut store = FileStore::new(dir.path().join("files")).unwrap();
        let entry = store
            .store_with(b"source", "original.TXT", "text/plain", |_, _| {
                (Some("context".to_string()), None)
            })
            .unwrap();
        let blob = entry.blob_metadata();
        let presentation = entry.presentation();

        store.rename(&entry.id, "renamed.txt").unwrap();
        store
            .set_tags(&entry.id, vec!["session-local-later".to_string()])
            .unwrap();

        assert_eq!(blob.id, entry.id);
        assert_eq!(blob.size, 6);
        assert_eq!(blob.extraction.version, crate::extract::EXTRACTION_VERSION);
        assert_eq!(presentation.display_name, "original.TXT");
        assert_eq!(presentation.declared_media_type, "text/plain");
        assert_eq!(presentation.extension, "txt");
        assert!(presentation.tags.is_empty());
        assert_eq!(store.read_bytes(&entry.id).unwrap(), b"source");
        assert_eq!(store.read_bytes(&entry.id).unwrap(), b"source");
    }

    #[test]
    fn each_upload_can_own_distinct_presentation_for_one_blob() {
        let dir = tempdir().unwrap();
        let mut store = FileStore::new(dir.path().join("files")).unwrap();
        let first = store
            .store_with(b"same", "first.pdf", "application/pdf", |_, _| (None, None))
            .unwrap();
        let second = store
            .store_with(b"same", "second.txt", "text/plain", |_, _| {
                panic!("deduped blob must not run extraction again")
            })
            .unwrap();
        assert_eq!(first.blob_metadata(), second.blob_metadata());

        let first_ref = FilePresentation::from_upload("first.pdf", "application/pdf");
        let second_ref = FilePresentation::from_upload("../second.txt", "text/plain");
        assert_eq!(first_ref.display_name, "first.pdf");
        assert_eq!(second_ref.display_name, "second.txt");
        assert_eq!(first_ref.extension, "pdf");
        assert_eq!(second_ref.extension, "txt");
    }

    #[test]
    fn suspicious_extension_falls_back_to_bin() {
        let dir = tempdir().unwrap();
        let mut store = FileStore::new(dir.path().join("files")).unwrap();
        let entry = store
            .store_with(
                b"source",
                "archive.really-long-unsafe-extension",
                "x/test",
                |_, _| (None, None),
            )
            .unwrap();
        assert_eq!(entry.ext, "bin");
        assert!(dir
            .path()
            .join("files")
            .join(&entry.id[..2])
            .join(format!("{}.bin", entry.id))
            .is_file());
    }

    #[cfg(unix)]
    #[test]
    fn attachment_bytes_stay_bound_to_the_opened_root_after_path_swap() {
        let parent = tempdir().unwrap();
        let configured = parent.path().join("files");
        let opened = parent.path().join("opened-files");
        let mut store = FileStore::new(&configured).unwrap();
        let entry = store
            .store_with(b"trusted attachment", "note.txt", "text/plain", |_, _| {
                (None, None)
            })
            .unwrap();

        std::fs::rename(&configured, &opened).unwrap();
        let hostile_shard = configured.join(&entry.id[..2]);
        std::fs::create_dir_all(&hostile_shard).unwrap();
        std::fs::write(
            hostile_shard.join(format!("{}.{}", entry.id, entry.ext)),
            b"replacement attachment",
        )
        .unwrap();

        assert_eq!(store.read_bytes(&entry.id).unwrap(), b"trusted attachment");
    }

    #[test]
    fn content_addressed_read_rejects_modified_blob_bytes() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("files");
        let mut store = FileStore::new(&root).unwrap();
        let entry = store
            .store_with(b"trusted", "note.txt", "text/plain", |_, _| (None, None))
            .unwrap();
        std::fs::write(
            root.join(&entry.id[..2])
                .join(format!("{}.{}", entry.id, entry.ext)),
            b"poison!",
        )
        .unwrap();

        assert!(matches!(
            store.read_bytes(&entry.id),
            Err(MemoryError::Invalid(_))
        ));
    }

    #[test]
    fn legacy_sidecar_loads_with_unknown_extraction_version() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("files");
        let mut store = FileStore::new(&root).unwrap();
        let entry = store
            .store_with(b"legacy", "legacy.txt", "text/plain", |_, _| {
                (Some("old extraction".to_string()), None)
            })
            .unwrap();
        drop(store);

        let sidecar = root
            .join(&entry.id[..2])
            .join(format!("{}.meta.json", entry.id));
        let mut json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&sidecar).unwrap()).unwrap();
        json.as_object_mut().unwrap().remove("extraction");
        std::fs::write(&sidecar, serde_json::to_vec_pretty(&json).unwrap()).unwrap();

        let mut reopened = FileStore::new(root).unwrap();
        reopened.load_all().unwrap();
        let loaded = reopened.get(&entry.id).unwrap();
        assert_eq!(loaded.extraction.version, 0);
        let metadata = loaded.extraction.extracted_text.unwrap();
        assert!(!metadata.truncated);
        assert_eq!(metadata.stored_bytes, "old extraction".len() as u64);
        assert_eq!(metadata.source_bytes, None);
    }
}
