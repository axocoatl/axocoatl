//! Workspace knowledge exposed inside a Session. Source reads use the Session's
//! owned sandbox; the source index is an observed cache, never current authority.
use super::*;
use axocoatl_memory::knowledge::*;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionKnowledgeEdit {
    pub id: Option<String>,
    pub title: String,
    pub body: String,
    pub kind: KnowledgeKind,
    pub expected_revision: u64,
    #[serde(default)]
    pub links: Vec<KnowledgeLink>,
    #[serde(default)]
    pub sources: Vec<KnowledgeSource>,
}
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum KnowledgeFreshness {
    Current,
    Stale,
    Unavailable,
    Unreferenced,
}
#[derive(Debug, Clone, Serialize)]
pub struct SessionKnowledgeNote {
    #[serde(flatten)]
    pub note: KnowledgeRecord,
    pub freshness: KnowledgeFreshness,
    pub applicability: Vec<KnowledgeSourceStatus>,
    pub backlinks: Vec<KnowledgeBacklink>,
}
#[derive(Debug, Clone, Serialize)]
pub struct SessionKnowledgeSymbol {
    pub name: String,
    pub kind: String,
    pub line: usize,
}
#[derive(Debug, Clone, Serialize)]
pub struct SessionKnowledgeIndexEntry {
    pub path: String,
    pub sha256: String,
    pub symbols: Vec<SessionKnowledgeSymbol>,
    pub language: Option<String>,
    pub parser_version: Option<String>,
    pub parse_status: SourceParseStatus,
    pub truncated: bool,
    pub imports: Vec<SourceImport>,
}
#[derive(Debug, Clone, Serialize)]
pub struct SessionKnowledgeIndex {
    pub status: String,
    pub files: usize,
    pub symbols: usize,
    pub entries: Vec<SessionKnowledgeIndexEntry>,
    pub snapshot_id: Option<String>,
    pub manifest_sha256: Option<String>,
    pub observed_at_unix_ms: Option<u64>,
    pub notice: String,
}
#[derive(Debug, Clone, Serialize)]
pub struct SessionKnowledgeView {
    pub workspace_id: String,
    pub notes: Vec<SessionKnowledgeNote>,
    pub proposals: Vec<KnowledgeProposal>,
    pub code_index: SessionKnowledgeIndex,
}
fn knowledge_error(error: impl std::fmt::Display) -> DaemonError {
    DaemonError::Session(error.to_string())
}
fn store_error(error: KnowledgeError) -> DaemonError {
    match error {
        KnowledgeError::Conflict { .. } | KnowledgeError::RecoveryRequired => {
            DaemonError::SessionConflict(error.to_string())
        }
        _ => knowledge_error(error),
    }
}
fn lock_store(
    store: &Arc<StdMutex<KnowledgeStore>>,
) -> Result<std::sync::MutexGuard<'_, KnowledgeStore>, DaemonError> {
    store
        .lock()
        .map_err(|_| knowledge_error("Workspace knowledge store lock failed"))
}
fn freshness(statuses: &[KnowledgeSourceStatus]) -> KnowledgeFreshness {
    if statuses.is_empty() {
        KnowledgeFreshness::Unreferenced
    } else if statuses
        .iter()
        .any(|s| s.status == SourceApplicability::Changed)
    {
        KnowledgeFreshness::Stale
    } else if statuses
        .iter()
        .any(|s| s.status == SourceApplicability::Unavailable)
    {
        KnowledgeFreshness::Unavailable
    } else {
        KnowledgeFreshness::Current
    }
}
fn project_note(
    note: KnowledgeRecord,
    all: &[KnowledgeRecord],
    sources: &SnapshotSources,
) -> SessionKnowledgeNote {
    let applicability = note.applicability(sources);
    let backlinks = all
        .iter()
        .flat_map(|other| {
            other
                .links
                .iter()
                .filter(|link| link.target == note.id)
                .map(|link| KnowledgeBacklink {
                    id: other.id.clone(),
                    title: other.title.clone(),
                    revision: other.revision,
                    kind: link.kind,
                })
        })
        .collect();
    SessionKnowledgeNote {
        freshness: freshness(&applicability),
        applicability,
        backlinks,
        note,
    }
}
fn index_view(index: Option<SourceIndex>) -> SessionKnowledgeIndex {
    let mut view=SessionKnowledgeIndex{status:"not_indexed".into(),files:0,symbols:0,entries:vec![],snapshot_id:None,manifest_sha256:None,observed_at_unix_ms:None,notice:"Observed source cache. Re-read a file to verify its current content. Imports are syntax references, not resolved dependencies or a call graph. Refresh is bounded to 512 text files, 256 KiB per file and 4 MiB total; generated, hidden and dependency directories are excluded.".into()};
    if let Some(index) = index {
        view.observed_at_unix_ms = Some(index.observed_at_unix_ms);
        view.status = "observed".into();
        view.snapshot_id = Some(index.snapshot_id);
        view.manifest_sha256 = Some(index.manifest_sha256);
        view.entries = index
            .files
            .into_iter()
            .map(|file| SessionKnowledgeIndexEntry {
                path: file.path,
                sha256: file.sha256,
                symbols: file
                    .definitions
                    .into_iter()
                    .map(|s| SessionKnowledgeSymbol {
                        name: s.symbol,
                        kind: s.kind,
                        line: s.line,
                    })
                    .collect(),
                language: file.language,
                parser_version: file.parser_version,
                parse_status: file.parse_status,
                truncated: file.truncated,
                imports: file.imports,
            })
            .collect();
        view.files = view.entries.len();
        view.symbols = view.entries.iter().map(|f| f.symbols.len()).sum();
    }
    view
}
impl AxocoatlDaemon {
    pub(crate) fn workspace_knowledge_store(
        &self,
        workspace_id: &str,
    ) -> Result<Arc<StdMutex<KnowledgeStore>>, DaemonError> {
        let mut cache = self
            .knowledge_stores
            .lock()
            .map_err(|_| knowledge_error("Workspace knowledge cache lock failed"))?;
        if let Some(store) = cache.get(workspace_id) {
            return Ok(store.clone());
        }
        let root = self.data_root.child(
            Path::new("memory/knowledge").join(axocoatl_memory::storage_key(workspace_id)),
        )?;
        let mut store = KnowledgeStore::open(root).map_err(store_error)?;
        store.bind_workspace(workspace_id).map_err(store_error)?;
        let store = Arc::new(StdMutex::new(store));
        cache.insert(workspace_id.into(), store.clone());
        Ok(store)
    }
    pub(crate) async fn session_knowledge_store(
        &self,
        id: &str,
    ) -> Result<Arc<StdMutex<KnowledgeStore>>, DaemonError> {
        let session = self
            .get_session(id)
            .await
            .ok_or_else(|| knowledge_error("Session does not exist"))?;
        if self.get_workspace(&session.workspace_id).await.is_none() {
            return Err(knowledge_error("Workspace does not exist"));
        }
        let store = self.workspace_knowledge_store(&session.workspace_id)?;
        if self.uses_native_session_history() {
            self.restore_native_lifecycle_history(
                &session,
                session.status == axocoatl_session::SessionStatus::Closed,
            )?;
            // Keep canonical/controller -> knowledge lock ordering. The canonical
            // snapshot is read under its actual retained owner; no ambient reopen.
            self.session_dispatch_lifecycles
                .with_session_knowledge_history(id, |canonical, content| {
                    let mut knowledge = lock_store(&store)?;
                    for proposal in knowledge.proposals().map_err(store_error)? {
                        let Some(snapshot) =
                            knowledge_publication_snapshot(canonical, content, &proposal)?
                        else {
                            continue;
                        };
                        match knowledge.publish(&proposal.id, &snapshot) {
                            Ok(_) | Err(KnowledgeError::Conflict { .. }) => {}
                            // As at turn close: a proposal that cannot be
                            // published stays pending and reviewable instead
                            // of making the whole knowledge view fail.
                            Err(KnowledgeError::Capacity) => {
                                tracing::warn!(proposal = %proposal.id, "knowledge proposal left pending: capacity");
                            }
                            Err(error) => return Err(store_error(error)),
                        }
                    }
                    Ok(())
                })?;
        }
        Ok(store)
    }
    async fn knowledge_live_sources(&self, id: &str, notes: &[KnowledgeRecord]) -> SnapshotSources {
        let paths: BTreeSet<_> = notes
            .iter()
            .flat_map(|n| n.sources.iter().map(|s| s.path.clone()))
            .collect();
        let mut sources = BTreeMap::new();
        // Bounded independently of how many links appear in the durable corpus.
        for path in paths.into_iter().take(128) {
            if let Ok(file) = self.session_read_file(id, &path).await {
                if !file.truncated
                    && !file.content.contains('\0')
                    && !file.content.contains('\u{fffd}')
                {
                    sources.insert(path, knowledge_content_hash(&file.content));
                }
            }
        }
        sources
    }
    pub async fn session_knowledge(
        &self,
        id: &str,
        query: Option<String>,
    ) -> Result<SessionKnowledgeView, DaemonError> {
        let store = self.session_knowledge_store(id).await?;
        let (workspace_id, all, proposals, index) = {
            let store = lock_store(&store)?;
            (
                store.workspace_id().unwrap_or_default().to_string(),
                store.list().map_err(store_error)?,
                store.proposals().map_err(store_error)?,
                store.source_index(id).map_err(store_error)?,
            )
        };
        let query = query.unwrap_or_default();
        if query.len() > 1024 {
            return Err(knowledge_error("Knowledge query exceeds 1024 bytes"));
        }
        let query = query.trim().to_lowercase();
        let selected: Vec<_> = all
            .iter()
            .filter(|n| {
                query.is_empty()
                    || n.title.to_lowercase().contains(&query)
                    || n.body.to_lowercase().contains(&query)
                    || n.sources.iter().any(|s| {
                        s.path.to_lowercase().contains(&query)
                            || s.symbol
                                .as_ref()
                                .is_some_and(|v| v.to_lowercase().contains(&query))
                    })
            })
            .cloned()
            .collect();
        let live = self.knowledge_live_sources(id, &selected).await;
        let notes = selected
            .into_iter()
            .map(|note| project_note(note, &all, &live))
            .collect();
        Ok(SessionKnowledgeView {
            workspace_id,
            notes,
            proposals,
            code_index: index_view(index),
        })
    }
    pub async fn save_session_knowledge(
        &self,
        id: &str,
        edit: SessionKnowledgeEdit,
    ) -> Result<SessionKnowledgeNote, DaemonError> {
        let store = self.session_knowledge_store(id).await?;
        let draft = KnowledgeDraft {
            id: edit.id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
            title: edit.title,
            body: edit.body,
            kind: edit.kind,
            links: edit.links,
            sources: edit.sources,
            provenance: KnowledgeProvenance::Human { author: None },
        };
        let (note, all) = {
            let mut store = lock_store(&store)?;
            let note = store
                .save(draft, edit.expected_revision)
                .map_err(store_error)?;
            (note, store.list().map_err(store_error)?)
        };
        let sources = self
            .knowledge_live_sources(id, std::slice::from_ref(&note))
            .await;
        Ok(project_note(note, &all, &sources))
    }
    pub async fn decide_session_knowledge(
        &self,
        id: &str,
        proposal_id: &str,
        accept: bool,
        expected_revision: Option<u64>,
    ) -> Result<SessionKnowledgeView, DaemonError> {
        let store = self.session_knowledge_store(id).await?;
        {
            let mut store = lock_store(&store)?;
            if accept {
                let revision = expected_revision.ok_or_else(|| {
                    knowledge_error("Accept requires the reviewed expected revision")
                })?;
                store
                    .accept_human(proposal_id, revision)
                    .map_err(store_error)?;
            } else {
                store.reject(proposal_id).map_err(store_error)?;
            }
        }
        self.session_knowledge(id, None).await
    }
    pub async fn preview_session_knowledge_import(
        &self,
        id: &str,
        markdown: String,
    ) -> Result<SessionKnowledgeEdit, DaemonError> {
        let parsed = parse_knowledge_markdown(&markdown).map_err(store_error)?;
        let store = self.session_knowledge_store(id).await?;
        let expected_revision = match lock_store(&store)?.read(&parsed.id, None) {
            Ok(_) => parsed.revision,
            Err(KnowledgeError::NotFound(_)) => 0,
            Err(error) => return Err(store_error(error)),
        };
        Ok(SessionKnowledgeEdit {
            id: Some(parsed.id),
            title: parsed.title,
            body: parsed.body,
            kind: parsed.kind,
            links: parsed.links,
            sources: parsed.sources,
            expected_revision,
        })
    }
    pub async fn export_session_knowledge_note(
        &self,
        id: &str,
        note_id: &str,
    ) -> Result<String, DaemonError> {
        let store = self.session_knowledge_store(id).await?;
        let result = lock_store(&store)?
            .export(note_id, None)
            .map_err(store_error);
        result
    }
    pub async fn export_session_knowledge(&self, id: &str) -> Result<String, DaemonError> {
        let store = self.session_knowledge_store(id).await?;
        let store = lock_store(&store)?;
        let mut text=String::from("# Workspace knowledge export\n\nEach note below retains its portable metadata. Use Export note for one-file import.\n\n");
        for note in store.list().map_err(store_error)? {
            let markdown = store
                .export(&note.id, Some(note.revision))
                .map_err(store_error)?;
            if text.len() + markdown.len() > 8 * 1024 * 1024 {
                return Err(knowledge_error(
                    "Collection exceeds 8 MiB; export individual notes",
                ));
            }
            text.push_str(&markdown);
            text.push_str("\n\n");
        }
        Ok(text)
    }
    pub async fn attach_session_knowledge(
        &self,
        id: &str,
        note_id: &str,
        expected_revision: u64,
    ) -> Result<SessionAttachmentRef, DaemonError> {
        let store = self.session_knowledge_store(id).await?;
        let markdown = lock_store(&store)?
            .export(note_id, Some(expected_revision))
            .map_err(store_error)?;
        let name = format!("knowledge-{note_id}-r{expected_revision}.md");
        let entry = self
            .file_store
            .lock()
            .await
            .store_with(markdown.as_bytes(), &name, "text/markdown", |_, _| {
                (Some(markdown.clone()), None)
            })
            .map_err(knowledge_error)?;
        self.create_session_attachment(CreateSessionAttachmentRef {
            reference_id: None,
            session_id: id.into(),
            blob_id: format!("sha256:{}", entry.id),
            display_name: name,
            declared_mime: Some("text/markdown".into()),
            size: entry.size,
            scope: TurnContextScope::ThisTurn,
            extraction: axocoatl_session::SessionAttachmentExtractionSnapshot {
                status: axocoatl_session::SessionAttachmentExtractionStatus::Ready,
                extractor: Some("workspace-knowledge-v1".into()),
                extracted_char_count: Some(markdown.chars().count() as u64),
                ..Default::default()
            },
            metadata: serde_json::Map::from_iter([
                ("knowledge_id".into(), serde_json::json!(note_id)),
                (
                    "knowledge_revision".into(),
                    serde_json::json!(expected_revision),
                ),
                (
                    "knowledge_sha256".into(),
                    serde_json::json!(knowledge_content_hash(&markdown)),
                ),
            ]),
        })
        .await
    }
    pub async fn refresh_session_knowledge_index(
        &self,
        id: &str,
    ) -> Result<SessionKnowledgeView, DaemonError> {
        let store = self.session_knowledge_store(id).await?;
        let mut pending = vec![String::new()];
        let mut files = Vec::new();
        let mut bytes = 0usize;
        let mut directories = 0usize;
        while let Some(path) = pending.pop() {
            directories += 1;
            if directories > 256 || files.len() >= 512 {
                break;
            }
            let entries = self
                .session_list_directory(id, if path.is_empty() { None } else { Some(&path) })
                .await?;
            for entry in entries {
                if entry.name.starts_with('.')
                    || matches!(
                        entry.name.as_str(),
                        "node_modules"
                            | "target"
                            | "vendor"
                            | "dist"
                            | "build"
                            | "venv"
                            | "__pycache__"
                    )
                {
                    continue;
                }
                if entry.kind == "dir" {
                    pending.push(entry.path);
                    continue;
                }
                if entry.kind != "file" || entry.size > 256 * 1024 || !indexable_path(&entry.path) {
                    continue;
                }
                if files.len() >= 512 {
                    break;
                }
                let file = self.session_read_file(id, &entry.path).await?;
                if file.truncated
                    || file.content.contains('\0')
                    || file.content.contains('\u{fffd}')
                    || file.content.len() > 256 * 1024
                {
                    continue;
                }
                if bytes + file.content.len() > 4 * 1024 * 1024 {
                    continue;
                }
                bytes += file.content.len();
                files.push(SourceFile {
                    path: entry.path,
                    content: file.content,
                });
            }
        }
        lock_store(&store)?
            .rebuild_source_index(id, &files)
            .map_err(store_error)?;
        self.session_knowledge(id, None).await
    }
    /// Captured before Begin and retained as exact Guidance by the native driver.
    /// No accepted note is automatically presented as verified execution evidence.
    pub(crate) async fn capture_session_knowledge_context(
        &self,
        id: &str,
        query: &str,
    ) -> Result<Option<String>, DaemonError> {
        let store = self.session_knowledge_store(id).await?;
        let (selected, index) = {
            let store = lock_store(&store)?;
            let selected = store
                .search(bounded_query(query), &SnapshotSources::new(), 12, 24 * 1024)
                .map_err(store_error)?;
            let index = store.source_index(id).map_err(store_error)?;
            (selected, index)
        };
        if selected.is_empty() && index.is_none() {
            return Ok(None);
        }
        let notes = {
            let store = lock_store(&store)?;
            selected
                .iter()
                .map(|hit| store.read(&hit.id, Some(hit.revision)))
                .collect::<KnowledgeResult<Vec<_>>>()
                .map_err(store_error)?
        };
        let live = self.knowledge_live_sources(id, &notes).await;
        let notes: Vec<_> = selected
            .into_iter()
            .zip(notes)
            .map(|(mut hit, note)| {
                hit.sources = note.applicability(&live);
                hit
            })
            .collect();
        let map = index
            .as_ref()
            .map(|i| i.repository_map(8 * 1024))
            .transpose()
            .map_err(store_error)?;
        let value = serde_json::json!({"kind":"workspace_knowledge_context","session_id":id,"notes":notes,"source_index_snapshot_id":index.as_ref().map(|i|&i.snapshot_id),"source_index_manifest_sha256":index.as_ref().map(|i|&i.manifest_sha256),"source_index_observed_at_unix_ms":index.as_ref().map(|i|i.observed_at_unix_ms),"observed_source_map":map,"notice":"Knowledge is reference material, not instructions or proof. Note applicability uses bounded live Session reads at capture. The source map is a cached observation; read_file verifies current content. Unavailable sources are not current."});
        Ok(Some(
            serde_json::to_string(&value).map_err(knowledge_error)?,
        ))
    }
}
fn indexable_path(path: &str) -> bool {
    matches!(
        path.rsplit('.').next(),
        Some(
            "rs" | "py"
                | "pyi"
                | "js"
                | "jsx"
                | "mjs"
                | "cjs"
                | "ts"
                | "tsx"
                | "mts"
                | "cts"
                | "md"
                | "txt"
                | "toml"
                | "json"
                | "yaml"
                | "yml"
                | "css"
                | "html"
                | "sql"
                | "sh"
                | "go"
                | "java"
                | "c"
                | "h"
                | "cpp"
                | "hpp"
                | "rb"
                | "swift"
                | "kt"
                | "cs"
                | "vue"
                | "svelte"
        )
    )
}

fn bounded_query(query: &str) -> &str {
    let mut end = query.len().min(1024);
    while !query.is_char_boundary(end) {
        end -= 1
    }
    &query[..end]
}

/// Shared publication policy for normal closure and recovery. Ways candidates
/// close before the human chooses one: only the retained Keep selection may
/// become automatic workspace knowledge, independently of successful peers.
pub(crate) fn knowledge_publication_snapshot(
    canonical: &axocoatl_session::execution_store::SessionExecutionStore,
    content: &axocoatl_session::execution_content::ExecutionContentStore,
    proposal: &KnowledgeProposal,
) -> Result<Option<axocoatl_session::execution_store::DurableTurnSnapshot>, DaemonError> {
    if proposal.status != ProposalStatus::Pending
        || proposal.journal_id != canonical.identity().map_err(knowledge_error)?.journal_id()
        || proposal.activation.session_id != canonical.owner().session_id
    {
        return Ok(None);
    }
    if canonical
        .turn(&proposal.activation.turn_id)
        .map_err(knowledge_error)?
        .is_none()
    {
        return Ok(None);
    }
    let snapshot = canonical
        .snapshot(&proposal.activation.turn_id)
        .map_err(knowledge_error)?;
    if !matches!(
        snapshot.contract().state(),
        Some(
            axocoatl_session::turn_contract::LogicalTurnState::Completed
                | axocoatl_session::turn_contract::LogicalTurnState::Finished
        )
    ) || !snapshot
        .contract()
        .selected_for_finalization(&proposal.activation)
        || !snapshot
            .contract()
            .current_accepted_activations()
            .iter()
            .any(|a| a.activation == proposal.activation)
    {
        return Ok(None);
    }
    if let Some((_, admission)) = content
        .turn_admission(canonical, snapshot.turn_id())
        .map_err(knowledge_error)?
    {
        if let Ok(ways) =
            serde_json::from_str::<super::native_ways::NativeWaysAdmission>(&admission.source)
        {
            if ways.schema_version != 1
                || ways.session_id != canonical.owner().session_id.as_str()
                || ways.source_turn_id != *snapshot.turn_id()
                || ways.request != admission.request
                || !ways
                    .candidates
                    .iter()
                    .any(|candidate| candidate.activation == proposal.activation)
            {
                return Err(knowledge_error(
                    "Knowledge proposal differs from its exact Ways admission",
                ));
            }
            if !content
                .selected_way_activations(canonical)
                .map_err(knowledge_error)?
                .contains(&proposal.activation)
            {
                return Ok(None);
            }
        } else if serde_json::from_str::<serde_json::Value>(&admission.source)
            .ok()
            .is_some_and(|value| value.get("set_id").is_some() || value.get("candidates").is_some())
        {
            return Err(knowledge_error(
                "Malformed Ways admission cannot authorize knowledge publication",
            ));
        }
    }
    Ok(Some(snapshot))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn knowledge_query_bounds_preserve_unicode_and_freshness_never_uses_cached_index() {
        assert!(bounded_query(&"🦎".repeat(400)).len() <= 1024);
        let record = KnowledgeRecord {
            id: "note".into(),
            title: "Decision".into(),
            body: "Context".into(),
            kind: KnowledgeKind::Decision,
            revision: 1,
            links: vec![],
            sources: vec![KnowledgeSource {
                path: "src.rs".into(),
                sha256: knowledge_content_hash("old"),
                symbol: None,
                role: Default::default(),
            }],
            provenance: KnowledgeProvenance::Human { author: None },
            acceptance: KnowledgeAcceptance::Human,
        };
        assert_eq!(
            project_note(record.clone(), &[], &SnapshotSources::new()).freshness,
            KnowledgeFreshness::Unavailable
        );
        assert_eq!(
            project_note(
                record.clone(),
                &[],
                &SnapshotSources::from([("src.rs".into(), knowledge_content_hash("new"))])
            )
            .freshness,
            KnowledgeFreshness::Stale
        );
        assert_eq!(
            project_note(
                record,
                &[],
                &SnapshotSources::from([("src.rs".into(), knowledge_content_hash("old"))])
            )
            .freshness,
            KnowledgeFreshness::Current
        );
    }
    /// Real daemon/store/attachment API seam in an isolated child. The tiny
    /// Podman fixture answers bootstrap probes only: it cannot read/write source.
    #[cfg(unix)]
    #[tokio::test]
    async fn knowledge_session_api_revision_import_attachment_and_workspace_isolation() {
        use std::os::unix::fs::PermissionsExt;
        const CHILD: &str = "AXOCOATL_TEST_KNOWLEDGE_CHILD";
        if std::env::var_os(CHILD).is_some() {
            let mut config =
                axocoatl_config::parse_config("agents: []\n", Path::new("fixture.yaml")).unwrap();
            config.consolidation.enabled = false;
            let daemon = AxocoatlDaemon::bootstrap_headless(config).await.unwrap();
            let cwd = std::env::current_dir().unwrap();
            let workspace = daemon
                .create_workspace(cwd.to_str().unwrap(), Some("Knowledge fixture"))
                .await
                .unwrap();
            let axocoatl_session::execution_ownership::DataRootFormatOwnership::Upgraded(ownership) =
                &daemon._data_dir_lease.ownership
            else {
                panic!("fresh native owner")
            };
            let (session, receipt) = daemon
                .session_store
                .lock()
                .await
                .create_native_with_environment(
                    ownership,
                    "Knowledge session",
                    &workspace.id,
                    &cwd,
                    SessionMode::Custom { agents: vec![] },
                    vec![],
                    vec![],
                    None,
                    None,
                    false,
                    false,
                )
                .unwrap();
            daemon
                .session_dispatch_lifecycles
                .retain_native_session(ownership.clone(), receipt)
                .unwrap();
            let edit = SessionKnowledgeEdit {
                id: Some("decision".into()),
                title: "Persist decisions".into(),
                body: "Original decision".into(),
                kind: KnowledgeKind::Decision,
                expected_revision: 0,
                links: vec![],
                sources: vec![],
            };
            let one = daemon
                .save_session_knowledge(&session.id, edit.clone())
                .await
                .unwrap();
            assert_eq!(one.note.revision, 1);
            assert_eq!(one.freshness, KnowledgeFreshness::Unreferenced);
            let markdown = daemon
                .export_session_knowledge_note(&session.id, "decision")
                .await
                .unwrap();
            let mut next = edit;
            next.expected_revision = 1;
            next.body = "Human correction".into();
            daemon
                .save_session_knowledge(&session.id, next.clone())
                .await
                .unwrap();
            assert!(matches!(
                daemon.save_session_knowledge(&session.id, next).await,
                Err(DaemonError::SessionConflict(_))
            ));
            let stale = daemon
                .preview_session_knowledge_import(&session.id, markdown.clone())
                .await
                .unwrap();
            assert_eq!(stale.expected_revision, 1);
            assert!(matches!(
                daemon.save_session_knowledge(&session.id, stale).await,
                Err(DaemonError::SessionConflict(_))
            ));
            assert_eq!(
                daemon
                    .session_knowledge(&session.id, None)
                    .await
                    .unwrap()
                    .notes[0]
                    .note
                    .body,
                "Human correction"
            );
            let attached = daemon
                .attach_session_knowledge(&session.id, "decision", 1)
                .await
                .unwrap();
            assert_eq!(attached.metadata["knowledge_revision"], 1);
            let bytes = daemon
                .file_store
                .lock()
                .await
                .read_bytes(AxocoatlDaemon::raw_blob_id(&attached.blob_id))
                .unwrap();
            assert_eq!(bytes, markdown.as_bytes());
            let view = daemon.session_knowledge(&session.id, None).await.unwrap();
            assert_eq!(view.code_index.status, "not_indexed");
            let context = daemon
                .capture_session_knowledge_context(&session.id, "Human correction")
                .await
                .unwrap()
                .unwrap();
            assert!(context.contains("Human correction"));
            assert!(daemon
                .capture_session_knowledge_context(&session.id, &"🦎".repeat(400))
                .await
                .is_ok());
            assert!(daemon
                .session_knowledge("not-a-session", None)
                .await
                .is_err());
            let other = daemon
                .workspace_knowledge_store("different-workspace")
                .unwrap();
            assert!(lock_store(&other).unwrap().list().unwrap().is_empty());
            daemon.close_session(&session.id).await.unwrap();
            let closed = daemon.session_knowledge(&session.id, None).await.unwrap();
            assert_eq!(closed.notes[0].note.revision, 2);
            daemon.shutdown().await.unwrap();
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let bin = root.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let podman = bin.join("podman");
        std::fs::write(&podman,"#!/bin/sh\ncase \"$*\" in\n --version) printf 'podman version 5.0.0\\n' ;;\n 'machine list --format json') printf '[{\"Running\":true}]\\n' ;;\n 'info --format json') printf '{}\\n' ;;\n 'ps '*) ;;\n *) exit 1 ;;\nesac\n").unwrap();
        std::fs::set_permissions(podman, std::fs::Permissions::from_mode(0o700)).unwrap();
        let result=tokio::time::timeout(Duration::from_secs(60),tokio::process::Command::new(std::env::current_exe().unwrap()).args(["--exact","bootstrap::session_knowledge::tests::knowledge_session_api_revision_import_attachment_and_workspace_isolation","--nocapture"]).env(CHILD,"1").env("AXOCOATL_DATA_DIR",root.path().join("data")).env("AXOCOATL_SOCKET_PATH",root.path().join("ipc/daemon.sock")).env("PATH",bin).current_dir(root.path()).kill_on_drop(true).output()).await.unwrap().unwrap();
        assert!(
            result.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
    }
}
