//! Rebuildable snapshot-scoped source index. Definitions and imports come from
//! pinned Tree-sitter syntax trees; imports are syntax references, not resolved
//! call graphs, type resolution, or a claim about build-system module paths.
use super::*;
use std::time::{Duration, Instant};
use tree_sitter::{Language, Node, ParseOptions, Parser};

const INDEX_BYTES: usize = 16 * 1024 * 1024;
const SOURCE_BYTES: usize = 4 * 1024 * 1024;
const FILE_BYTES: usize = 256 * 1024;
const FILE_COUNT: usize = 512;
const NODE_COUNT: usize = 200_000;
const ITEMS_PER_FILE: usize = 2048;
const INDEX_VERSION: &str = "tree-sitter-0.25.10/index-v1";
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceFile {
    pub path: String,
    pub content: String,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceParseStatus {
    Parsed,
    Partial,
    Unsupported,
    TimedOut,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceDefinition {
    pub name: String,
    pub kind: String,
    pub symbol: String,
    pub line: usize,
    pub column: usize,
    pub end_line: usize,
    pub start_byte: usize,
    pub end_byte: usize,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceImport {
    pub kind: String,
    pub text: String,
    pub line: usize,
    /// Syntax only. No inferred module resolution is represented by this index.
    pub resolved: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IndexedSourceFile {
    pub path: String,
    pub sha256: String,
    pub content: String,
    pub language: Option<String>,
    pub parser_version: Option<String>,
    pub parse_status: SourceParseStatus,
    pub definitions: Vec<SourceDefinition>,
    pub imports: Vec<SourceImport>,
    pub truncated: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceIndex {
    pub schema: u32,
    pub workspace_id: String,
    pub snapshot_id: String,
    pub index_version: String,
    pub manifest_sha256: String,
    pub observed_at_unix_ms: u64,
    pub files: Vec<IndexedSourceFile>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceSearchHit {
    pub path: String,
    pub sha256: String,
    pub line: usize,
    pub excerpt: String,
    pub symbol: Option<String>,
}
impl SourceIndex {
    pub fn sources(&self) -> SnapshotSources {
        self.files
            .iter()
            .map(|f| (f.path.clone(), f.sha256.clone()))
            .collect()
    }
    pub fn search(
        &self,
        query: &str,
        limit: usize,
        max_bytes: usize,
    ) -> KnowledgeResult<Vec<SourceSearchHit>> {
        if query.len() > 1024 || limit > 100 || max_bytes > 64 * 1024 {
            return Err(KnowledgeError::Capacity);
        }
        let query = query.trim().to_lowercase();
        let mut result = Vec::new();
        if query.is_empty() {
            return Ok(result);
        }
        'files: for file in &self.files {
            for (line, text) in file.content.lines().enumerate() {
                if !text.to_lowercase().contains(&query)
                    && !file.path.to_lowercase().contains(&query)
                {
                    continue;
                }
                if result.len() >= limit {
                    break 'files;
                }
                result.push(SourceSearchHit {
                    path: file.path.clone(),
                    sha256: file.sha256.clone(),
                    line: line + 1,
                    excerpt: prefix(text, 1024).into(),
                    symbol: file
                        .definitions
                        .iter()
                        .find(|d| d.line == line + 1)
                        .map(|d| d.symbol.clone()),
                });
                if serde_json::to_vec(&result)?.len() > max_bytes {
                    result.pop();
                    break 'files;
                }
            }
        }
        Ok(result)
    }
    pub fn repository_map(&self, max_bytes: usize) -> KnowledgeResult<String> {
        if max_bytes > 64 * 1024 {
            return Err(KnowledgeError::Capacity);
        }
        let mut map = String::new();
        for file in &self.files {
            let row = format!("{} [{}; {:?}]\n", file.path, file.sha256, file.parse_status);
            if map.len() + row.len() > max_bytes {
                break;
            }
            map.push_str(&row);
            for d in &file.definitions {
                let row = format!("  {}:{} {} {}\n", file.path, d.line, d.kind, d.symbol);
                if map.len() + row.len() > max_bytes {
                    return Ok(map);
                }
                map.push_str(&row);
            }
        }
        Ok(map)
    }
}
impl KnowledgeStore {
    /// Caller captures these files under one exact Session/runtime/snapshot identity.
    /// No host filesystem traversal or candidate execution occurs in the indexer.
    pub fn rebuild_source_index(
        &mut self,
        snapshot_id: &str,
        files: &[SourceFile],
    ) -> KnowledgeResult<SourceIndex> {
        self.bound()?;
        validate_text(snapshot_id, 1024, "snapshot identity")?;
        if files.len() > FILE_COUNT
            || files.iter().map(|f| f.content.len()).sum::<usize>() > SOURCE_BYTES
        {
            return Err(KnowledgeError::Capacity);
        }
        let mut sorted: Vec<_> = files.iter().collect();
        sorted.sort_by(|a, b| a.path.cmp(&b.path));
        let mut paths = BTreeSet::new();
        for file in &sorted {
            validate_path(&file.path)?;
            if file.content.len() > FILE_BYTES || !paths.insert(&file.path) {
                return Err(invalid("duplicate or oversized indexed source"));
            }
        }
        let manifest: SnapshotSources = sorted
            .iter()
            .map(|f| (f.path.clone(), digest(f.content.as_bytes())))
            .collect();
        let manifest_sha256 = digest(&serde_json::to_vec(&manifest)?);
        // An unchanged file can reuse a complete parse from a prior snapshot.
        let previous = self.source_index(snapshot_id).ok().flatten();
        let began = Instant::now();
        let mut indexed = Vec::new();
        for file in sorted {
            if let Some(cached) = previous.as_ref().and_then(|i| {
                i.files.iter().find(|f| {
                    f.path == file.path
                        && f.sha256 == manifest[&file.path]
                        && f.parse_status != SourceParseStatus::TimedOut
                })
            }) {
                indexed.push(cached.clone());
                continue;
            }
            indexed.push(parse_file(file, began.elapsed() > Duration::from_secs(5))?);
        }
        let index = SourceIndex {
            schema: 1,
            workspace_id: self.workspace_id().expect("bound workspace").into(),
            snapshot_id: snapshot_id.into(),
            index_version: INDEX_VERSION.into(),
            manifest_sha256,
            observed_at_unix_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|_| invalid("system clock precedes Unix epoch"))?
                .as_millis()
                .try_into()
                .map_err(|_| KnowledgeError::Capacity)?,
            files: indexed,
        };
        let bytes = serde_json::to_vec(&index)?;
        if bytes.len() > INDEX_BYTES {
            return Err(KnowledgeError::Capacity);
        }
        self.root.atomic_write(index_path(snapshot_id), &bytes)?;
        Ok(index)
    }
    pub fn source_index(&self, snapshot_id: &str) -> KnowledgeResult<Option<SourceIndex>> {
        self.bound()?;
        validate_text(snapshot_id, 1024, "snapshot identity")?;
        let path = index_path(snapshot_id);
        if !self.root.is_file(&path)? {
            return Ok(None);
        }
        let index: SourceIndex =
            serde_json::from_slice(&self.root.read_limited(path, INDEX_BYTES)?)?;
        if index.schema != 1
            || index.snapshot_id != snapshot_id
            || Some(index.workspace_id.as_str()) != self.workspace_id()
            || index.index_version != INDEX_VERSION
            || index.files.len() > FILE_COUNT
        {
            return Err(invalid(
                "source index identity/version mismatch; rebuild required",
            ));
        }
        let mut paths = BTreeSet::new();
        let mut size = 0usize;
        for file in &index.files {
            validate_path(&file.path)?;
            size = size.saturating_add(file.content.len());
            if !paths.insert(&file.path)
                || file.content.len() > FILE_BYTES
                || digest(file.content.as_bytes()) != file.sha256
                || file.definitions.len() > ITEMS_PER_FILE
                || file.imports.len() > ITEMS_PER_FILE
            {
                return Err(invalid("source index content mismatch; rebuild required"));
            }
        }
        if size > SOURCE_BYTES
            || digest(&serde_json::to_vec(&index.sources())?) != index.manifest_sha256
        {
            return Err(invalid("source index manifest mismatch; rebuild required"));
        }
        Ok(Some(index))
    }
}
pub(super) fn index_path(snapshot: &str) -> String {
    format!("indexes/{}.json", digest(snapshot.as_bytes()))
}
fn grammar(path: &str) -> Option<(&'static str, &'static str, Language)> {
    match path.rsplit('.').next()? {
        "rs" => Some((
            "rust",
            "tree-sitter-rust/0.24.2",
            tree_sitter_rust::LANGUAGE.into(),
        )),
        "js" | "jsx" | "mjs" | "cjs" => Some((
            "javascript",
            "tree-sitter-javascript/0.25.0",
            tree_sitter_javascript::LANGUAGE.into(),
        )),
        "ts" | "mts" | "cts" => Some((
            "typescript",
            "tree-sitter-typescript/0.23.2",
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        )),
        "tsx" => Some((
            "tsx",
            "tree-sitter-typescript/0.23.2",
            tree_sitter_typescript::LANGUAGE_TSX.into(),
        )),
        "py" | "pyi" => Some((
            "python",
            "tree-sitter-python/0.25.0",
            tree_sitter_python::LANGUAGE.into(),
        )),
        _ => None,
    }
}
fn parse_file(file: &SourceFile, skip_parse: bool) -> KnowledgeResult<IndexedSourceFile> {
    let mut out = IndexedSourceFile {
        path: file.path.clone(),
        sha256: digest(file.content.as_bytes()),
        content: file.content.clone(),
        language: None,
        parser_version: None,
        parse_status: SourceParseStatus::Unsupported,
        definitions: Vec::new(),
        imports: Vec::new(),
        truncated: false,
    };
    let Some((language, version, grammar)) = grammar(&file.path) else {
        return Ok(out);
    };
    out.language = Some(language.into());
    out.parser_version = Some(version.into());
    if skip_parse {
        out.parse_status = SourceParseStatus::TimedOut;
        return Ok(out);
    }
    let mut parser = Parser::new();
    parser
        .set_language(&grammar)
        .map_err(|e| invalid(&e.to_string()))?;
    let began = Instant::now();
    let mut progress = |_: &tree_sitter::ParseState| began.elapsed() > Duration::from_millis(500);
    let mut input = |offset: usize, _: tree_sitter::Point| &file.content.as_bytes()[offset..];
    let Some(tree) = parser.parse_with_options(
        &mut input,
        None,
        Some(ParseOptions::new().progress_callback(&mut progress)),
    ) else {
        out.parse_status = SourceParseStatus::TimedOut;
        return Ok(out);
    };
    out.parse_status = if tree.root_node().has_error() {
        SourceParseStatus::Partial
    } else {
        SourceParseStatus::Parsed
    };
    let mut stack = vec![(tree.root_node(), String::new())];
    let mut visited = 0usize;
    while let Some((node, scope)) = stack.pop() {
        visited += 1;
        if visited > NODE_COUNT {
            out.truncated = true;
            break;
        }
        // Omit explicit error nodes. A recovered surrounding declaration stays
        // visibly Partial at file level rather than claiming a validated symbol.
        if node.is_error() || node.is_missing() {
            continue;
        }
        let kind = node.kind();
        let mut next_scope = scope.clone();
        if definition_kind(kind) {
            if let Some(name) = node
                .child_by_field_name("name")
                .and_then(|n| node_text(n, &file.content))
            {
                if name.len() <= 512 && !name.trim().is_empty() {
                    let symbol = if scope.is_empty() {
                        name.to_string()
                    } else {
                        format!("{scope}::{name}")
                    };
                    if symbol.len() <= 1024 && out.definitions.len() < ITEMS_PER_FILE {
                        out.definitions.push(SourceDefinition {
                            name: name.into(),
                            kind: kind.into(),
                            symbol: symbol.clone(),
                            line: node.start_position().row + 1,
                            column: node.start_position().column + 1,
                            end_line: node.end_position().row + 1,
                            start_byte: node.start_byte(),
                            end_byte: node.end_byte(),
                        });
                    } else {
                        out.truncated = true
                    }
                    next_scope = symbol;
                }
            }
        }
        if matches!(
            kind,
            "import_statement"
                | "import_from_statement"
                | "use_declaration"
                | "extern_crate_declaration"
                | "import_alias"
        ) {
            if out.imports.len() < ITEMS_PER_FILE {
                let text = node_text(node, &file.content).unwrap_or("");
                if text.len() > 2048 {
                    out.truncated = true
                }
                out.imports.push(SourceImport {
                    kind: kind.into(),
                    text: prefix(text, 2048).into(),
                    line: node.start_position().row + 1,
                    resolved: false,
                });
            } else {
                out.truncated = true
            }
        }
        // Iterative traversal bounds stack depth even for adversarial nested source.
        let mut cursor = node.walk();
        let children: Vec<_> = node.named_children(&mut cursor).collect();
        for child in children.into_iter().rev() {
            stack.push((child, next_scope.clone()))
        }
    }
    Ok(out)
}
fn node_text<'a>(node: Node<'_>, source: &'a str) -> Option<&'a str> {
    source.get(node.byte_range())
}
fn definition_kind(kind: &str) -> bool {
    matches!(
        kind,
        "function_item"
            | "struct_item"
            | "enum_item"
            | "trait_item"
            | "type_item"
            | "const_item"
            | "static_item"
            | "mod_item"
            | "function_definition"
            | "class_definition"
            | "function_declaration"
            | "generator_function_declaration"
            | "class_declaration"
            | "method_definition"
            | "interface_declaration"
            | "type_alias_declaration"
            | "enum_declaration"
            | "variable_declarator"
    )
}
