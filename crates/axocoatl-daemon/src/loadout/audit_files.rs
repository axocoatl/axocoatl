//! The audit's host-verified coverage: the repository's files as the host
//! lists them, their assignment to the areas of the plan as executed, and
//! which of them each area worker read, judged from the tool calls its
//! Session recorded, never from what the worker answers.
//!
//! - **Listing** ([`list_repository`]): in a Git work tree, `git ls-files
//!   --cached --others --exclude-standard` (host Git with every
//!   command-bearing setting overridden); elsewhere, or when Git cannot
//!   list it, a walk that skips the directories `glob` skips
//!   ([`GLOB_SKIPPED_DIRECTORIES`]). Regular files only (no links,
//!   submodules or anything inside `.git`), at most [`MAX_LISTED_FILES`].
//! - **Kinds** ([`FileKind`]): empty; binary (a NUL byte in its first 8,000
//!   bytes, as Git decides); too large (over [`MAX_AUDITED_FILE_BYTES`]); or
//!   text, which its area's worker must read.
//! - **Assignment** ([`assign`]): each file goes to the first area whose
//!   paths match it; a file no area's paths match goes to the area whose
//!   paths share the most leading directories with it (the first such area
//!   on a tie), else to the host-made area [`REST_AREA`]. An area left
//!   without files is not run.
//! - **Coverage** ([`unread`]): a text file is read when a `read_file` call
//!   of it succeeded in an activation of its area's worker that answered:
//!   any such call when the file fits one read ([`READ_WINDOW_BYTES`]),
//!   else calls whose windows (each the 64 KiB, or its `limit`, from its
//!   `offset`) together cover the whole file. Empty and binary files need no read; a file too
//!   large is a note.
//!
//! Owner: audit.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;
use std::time::Duration;

use axocoatl_session::audit_plan::{AuditArea, AuditPlan};
use axocoatl_session::path_scope::{in_git_directory, pattern_matches};
use axocoatl_tools::fs_tools::{GLOB_SKIPPED_DIRECTORIES, READ_FILE_WINDOW_BYTES};

use crate::loadout::ToolCallRecord;

/// Most files the host lists; a repository with more has the rest not
/// covered ([`super::UNLISTED`]).
pub const MAX_LISTED_FILES: usize = 20_000;
/// Largest file a worker must read; a larger one is not read (a note).
pub const MAX_AUDITED_FILE_BYTES: u64 = 256 * 1024;
/// The bytes one `read_file` call reads: the window from its `offset`.
pub const READ_WINDOW_BYTES: u64 = READ_FILE_WINDOW_BYTES as u64;
/// How much of a file is looked at for a NUL byte (Git's rule).
const BINARY_PROBE_BYTES: usize = 8_000;
/// The host-made area of files no planned area takes.
pub const REST_AREA: &str = "rest";
/// The longest file list a worker's instructions carry; past it, the list
/// names directories with counts.
pub const MAX_FILE_LIST_BYTES: usize = 8 * 1024;
const HOST_GIT_TIMEOUT: Duration = Duration::from_secs(60);

/// What a listed file is, for coverage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    /// Its worker must read it.
    Text,
    /// No bytes: covered without a read.
    Empty,
    /// A NUL byte in its first 8,000 bytes: covered without a read.
    Binary,
    /// Over [`MAX_AUDITED_FILE_BYTES`]: not read, a note.
    TooLarge,
}

impl FileKind {
    /// The kind in a word, as the assignment shows it.
    pub fn label(self) -> &'static str {
        match self {
            FileKind::Text => "text",
            FileKind::Empty => "empty",
            FileKind::Binary => "binary",
            FileKind::TooLarge => "too large",
        }
    }

    /// How the coverage line says a file of this kind was not read.
    pub fn not_read(self) -> &'static str {
        match self {
            FileKind::Text => "not read",
            FileKind::Empty => "not read: empty",
            FileKind::Binary => "not read: binary",
            FileKind::TooLarge => "not read: too large",
        }
    }
}

/// One file of the repository as the host listed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoFile {
    /// Relative to the repository, `/`-separated.
    pub path: String,
    pub size: u64,
    pub kind: FileKind,
}

/// How the host listed the repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListingMethod {
    /// `git ls-files --cached --others --exclude-standard`.
    Git,
    /// A walk of the directory that skips what `glob` skips.
    Walk,
}

impl ListingMethod {
    pub fn describe(self) -> &'static str {
        match self {
            ListingMethod::Git => "git ls-files --cached --others --exclude-standard",
            ListingMethod::Walk => "a walk of the directory (not a Git work tree)",
        }
    }
}

/// The repository's files, sorted by path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoListing {
    pub files: Vec<RepoFile>,
    pub method: ListingMethod,
    /// More files than [`MAX_LISTED_FILES`]: the listing stops there.
    pub capped: bool,
    /// Files whose names are not UTF-8: no worker can name them to
    /// `read_file`, so they are left out and counted.
    pub unnamed: usize,
}

/// List the repository at `repo` on the host.
pub async fn list_repository(repo: &Path) -> Result<RepoListing, String> {
    list_repository_within(repo, MAX_LISTED_FILES).await
}

/// [`list_repository`] with at most `max` files.
async fn list_repository_within(repo: &Path, max: usize) -> Result<RepoListing, String> {
    let (paths, method) = match git_paths(repo).await {
        Some(paths) => (Some(paths), ListingMethod::Git),
        None => (None, ListingMethod::Walk),
    };
    let repo = repo.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let (paths, unnamed) = match paths {
            Some(paths) => paths,
            None => walk(&repo, max)?,
        };
        let mut listing = classify(&repo, paths, method, max);
        listing.unnamed = unnamed;
        Ok(listing)
    })
    .await
    .map_err(|error| format!("the listing task failed: {error}"))?
}

/// The paths `git ls-files` lists, and how many it listed whose names are
/// not UTF-8; `None` when Git cannot list `repo` (not a work tree, no Git, a
/// repository Git refuses).
async fn git_paths(repo: &Path) -> Option<(Vec<String>, usize)> {
    let mut command = crate::git_host::repository_git(
        &crate::git_host::HostTools::default(),
        repo,
        &[
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
            "-z",
        ],
    );
    command.stdin(std::process::Stdio::null());
    let output = tokio::time::timeout(HOST_GIT_TIMEOUT, command.output())
        .await
        .ok()?
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let mut paths = Vec::new();
    let mut unnamed = 0;
    for path in output.stdout.split(|byte| *byte == 0) {
        match std::str::from_utf8(path) {
            Ok("") => {}
            Ok(path) => paths.push(path.to_owned()),
            Err(_) => unnamed += 1,
        }
    }
    paths.sort();
    paths.dedup();
    Some((paths, unnamed))
}

/// Every regular file under `repo`, depth first in name order, skipping
/// [`GLOB_SKIPPED_DIRECTORIES`] and links; at most `max + 1` (one more
/// tells the listing it stopped). Also how many files and directories have
/// names that are not UTF-8, which it cannot list.
fn walk(repo: &Path, max: usize) -> Result<(Vec<String>, usize), String> {
    let mut found = Vec::new();
    let mut unnamed = 0;
    let mut pending = vec![String::new()];
    while let Some(directory) = pending.pop() {
        let full = if directory.is_empty() {
            repo.to_path_buf()
        } else {
            repo.join(&directory)
        };
        let read =
            std::fs::read_dir(&full).map_err(|error| format!("{}: {error}", full.display()))?;
        let mut entries = Vec::new();
        for entry in read {
            let entry = entry.map_err(|error| format!("{}: {error}", full.display()))?;
            let Ok(name) = entry.file_name().into_string() else {
                unnamed += 1;
                continue;
            };
            let kind = entry
                .file_type()
                .map_err(|error| format!("{}: {error}", entry.path().display()))?;
            entries.push((name, kind));
        }
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        let mut directories = Vec::new();
        for (name, kind) in entries {
            let child = if directory.is_empty() {
                name.clone()
            } else {
                format!("{directory}/{name}")
            };
            if kind.is_dir() {
                if !GLOB_SKIPPED_DIRECTORIES.contains(&name.as_str()) {
                    directories.push(child);
                }
            } else if kind.is_file() {
                found.push(child);
                if found.len() > max {
                    return Ok((found, unnamed));
                }
            }
        }
        pending.extend(directories.into_iter().rev());
    }
    Ok((found, unnamed))
}

/// The regular files among `paths`, each with its size and kind, at most
/// `max`.
fn classify(repo: &Path, paths: Vec<String>, method: ListingMethod, max: usize) -> RepoListing {
    let mut files = Vec::new();
    let mut capped = false;
    for path in paths {
        if path.starts_with('/')
            || in_git_directory(&path)
            || path.split('/').any(|part| part.is_empty() || part == "..")
        {
            continue;
        }
        let full = repo.join(&path);
        let Ok(metadata) = std::fs::symlink_metadata(&full) else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        if files.len() == max {
            capped = true;
            break;
        }
        let size = metadata.len();
        let kind = if size == 0 {
            FileKind::Empty
        } else if has_nul(&full) {
            FileKind::Binary
        } else if size > MAX_AUDITED_FILE_BYTES {
            FileKind::TooLarge
        } else {
            FileKind::Text
        };
        files.push(RepoFile { path, size, kind });
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    RepoListing {
        files,
        method,
        capped,
        unnamed: 0,
    }
}

/// A NUL byte in the file's first [`BINARY_PROBE_BYTES`]; a file the host
/// cannot read is taken as text, so its worker must read it.
fn has_nul(path: &Path) -> bool {
    use std::io::Read;
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    let mut probe = Vec::with_capacity(BINARY_PROBE_BYTES);
    if file
        .take(BINARY_PROBE_BYTES as u64)
        .read_to_end(&mut probe)
        .is_err()
    {
        return false;
    }
    probe.contains(&0)
}

/// One area of the plan as executed, with the files assigned to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssignedArea {
    pub area: AuditArea,
    /// Made by the host for files no planned area takes ([`REST_AREA`]).
    pub host_made: bool,
    pub files: Vec<RepoFile>,
    /// Of `files`, those no pattern of the area matches, which the host
    /// assigned to it because its paths share their leading directories.
    pub by_path: Vec<String>,
}

impl AssignedArea {
    /// The files its worker must read.
    pub fn text_files(&self) -> impl Iterator<Item = &RepoFile> {
        self.files.iter().filter(|file| file.kind == FileKind::Text)
    }
}

/// The plan as executed: the planned areas that have files, in plan order,
/// then the host-made area when it has any.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assignment {
    pub areas: Vec<AssignedArea>,
    /// Planned areas no file was assigned to; they are not run.
    pub without_files: Vec<String>,
}

impl Assignment {
    pub fn names(&self) -> Vec<&str> {
        self.areas
            .iter()
            .map(|assigned| assigned.area.name.as_str())
            .collect()
    }
}

/// Assign each of `files` to one area of `plan` ([`Assignment`]).
pub fn assign(plan: &AuditPlan, files: &[RepoFile]) -> Assignment {
    let mut buckets: Vec<(Vec<RepoFile>, Vec<String>)> = vec![Default::default(); plan.areas.len()];
    let mut rest = Vec::new();
    for file in files {
        if let Some(index) = plan.areas.iter().position(|area| {
            area.paths
                .iter()
                .any(|pattern| area_holds(pattern, &file.path))
        }) {
            buckets[index].0.push(file.clone());
            continue;
        }
        let mut best: Option<(usize, usize)> = None;
        for (index, area) in plan.areas.iter().enumerate() {
            let shared = shared_directories(area, &file.path);
            if shared > 0 && best.is_none_or(|(_, most)| shared > most) {
                best = Some((index, shared));
            }
        }
        match best {
            Some((index, _)) => {
                buckets[index].0.push(file.clone());
                buckets[index].1.push(file.path.clone());
            }
            None => rest.push(file.clone()),
        }
    }
    let mut areas = Vec::new();
    let mut without_files = Vec::new();
    for (area, (files, by_path)) in plan.areas.iter().zip(buckets) {
        if files.is_empty() {
            without_files.push(area.name.clone());
        } else {
            areas.push(AssignedArea {
                area: area.clone(),
                host_made: false,
                files,
                by_path,
            });
        }
    }
    if !rest.is_empty() {
        areas.push(AssignedArea {
            area: AuditArea {
                name: rest_name(plan),
                scope: "the files no planned area's paths name and that share no directory \
                        with them, made by the host so every file has an area: audit each \
                        one in its own right"
                    .into(),
                paths: Vec::new(),
            },
            host_made: true,
            files: rest,
            by_path: Vec::new(),
        });
    }
    Assignment {
        areas,
        without_files,
    }
}

/// [`REST_AREA`], or `rest-<n>` when the planner already named an area so.
fn rest_name(plan: &AuditPlan) -> String {
    let taken = |name: &str| plan.areas.iter().any(|area| area.name == name);
    if !taken(REST_AREA) {
        return REST_AREA.into();
    }
    (2..)
        .map(|number| format!("{REST_AREA}-{number}"))
        .find(|name| !taken(name))
        .unwrap_or_default()
}

/// A plan pattern as `pattern_matches` reads it: without a leading `./` or
/// `/` (both mean the repository's root).
fn normalized(pattern: &str) -> &str {
    let mut pattern = pattern.trim();
    while let Some(rest) = pattern.strip_prefix("./") {
        pattern = rest;
    }
    pattern.trim_start_matches('/')
}

/// Whether `path` lies inside an area `pattern`: matched by it, or, for a
/// pattern without wildcards, that path itself or under it as a directory
/// (`billing` holds `billing/pagination.py`).
pub fn area_holds(pattern: &str, path: &str) -> bool {
    let pattern = normalized(pattern);
    if pattern.is_empty() {
        return false;
    }
    if pattern_matches(pattern, path) {
        return true;
    }
    let literal = pattern.trim_end_matches('/');
    !literal.is_empty()
        && !literal.contains(['*', '?'])
        && (path == literal
            || path
                .strip_prefix(literal)
                .is_some_and(|rest| rest.starts_with('/')))
}

/// How many leading directories of `path` the literal start of one of the
/// area's patterns shares (`src/api/x.rs` shares one with `src/auth/**`). A
/// pattern without `/` names a file at any depth and shares none.
fn shared_directories(area: &AuditArea, path: &str) -> usize {
    let mut directories: Vec<&str> = path.split('/').collect();
    directories.pop();
    area.paths
        .iter()
        .map(|pattern| {
            let pattern = normalized(pattern);
            if !pattern.trim_end_matches('/').contains('/') && !pattern.ends_with('/') {
                return 0;
            }
            let literal = pattern
                .split('/')
                .filter(|part| !part.is_empty())
                .take_while(|part| !part.contains(['*', '?']));
            directories
                .iter()
                .zip(literal)
                .take_while(|(directory, part)| **directory == *part)
                .count()
        })
        .max()
        .unwrap_or(0)
}

/// `files` as lines of a worker's instructions or a record line: each path
/// on its own line when they fit in `max_bytes`, else the directories they
/// are in with how many of them each holds, as deep as fits. The flag says
/// whether directories stand for the files.
pub fn file_list(files: &[&str], max_bytes: usize) -> (String, bool) {
    let one_by_one: String = files.iter().map(|path| format!("- {path}\n")).collect();
    if one_by_one.len() <= max_bytes {
        return (one_by_one, false);
    }
    let deepest = files
        .iter()
        .map(|path| path.matches('/').count())
        .max()
        .unwrap_or(0);
    let mut text = String::new();
    for depth in (1..=deepest.max(1)).rev() {
        let mut groups: BTreeMap<&str, usize> = BTreeMap::new();
        for path in files {
            *groups.entry(directory_at(path, depth)).or_default() += 1;
        }
        text = groups
            .iter()
            .map(|(directory, count)| group_line(directory, *count))
            .collect();
        if text.len() <= max_bytes {
            return (text, true);
        }
        if depth == 1 {
            // Even the top directories do not fit: as many as fit, then a
            // count of the rest.
            text.clear();
            let mut shown = 0;
            let mut files_shown = 0;
            for (directory, count) in &groups {
                let line = group_line(directory, *count);
                if text.len() + line.len() + 64 > max_bytes {
                    break;
                }
                text.push_str(&line);
                shown += 1;
                files_shown += count;
            }
            let _ = writeln!(
                text,
                "- and {} more directories ({} files)",
                groups.len() - shown,
                files.len() - files_shown
            );
        }
    }
    (text, true)
}

/// The first `depth` directories of `path` (`""` for a file at the root).
fn directory_at(path: &str, depth: usize) -> &str {
    let mut end = 0;
    let mut seen = 0;
    for (index, character) in path.char_indices() {
        if character == '/' {
            end = index;
            seen += 1;
            if seen == depth {
                break;
            }
        }
    }
    &path[..end]
}

fn group_line(directory: &str, count: usize) -> String {
    let files = if count == 1 { "file" } else { "files" };
    if directory.is_empty() {
        format!("- the repository's root directory itself: {count} {files}\n")
    } else {
        format!("- {directory}/ and below: {count} {files}\n")
    }
}

/// The text files of `files` not read in `calls` (the calls of the
/// activations of their area's worker that answered).
pub fn unread<'a>(
    files: &'a [RepoFile],
    calls: &[&ToolCallRecord],
    repo: &Path,
) -> Vec<&'a RepoFile> {
    let mut windows: BTreeMap<String, Vec<(u64, u64)>> = BTreeMap::new();
    for call in calls {
        if call.tool != "read_file" || !call.succeeded {
            continue;
        }
        let Some(path) = call
            .arguments
            .get("path")
            .and_then(|value| value.as_str())
            .and_then(|raw| repo_relative(raw.trim(), repo))
            .filter(|path| !path.is_empty())
        else {
            continue;
        };
        let Some(offset) = read_number(call.arguments.get("offset")) else {
            continue;
        };
        let Some(length) = read_number(call.arguments.get("limit")) else {
            continue;
        };
        let length = if length == 0 {
            READ_WINDOW_BYTES
        } else {
            length.min(READ_WINDOW_BYTES)
        };
        windows
            .entry(path)
            .or_default()
            .push((offset, offset.saturating_add(length)));
    }
    files
        .iter()
        .filter(|file| file.kind == FileKind::Text)
        .filter(|file| !read_whole(file, windows.get(&file.path).map(Vec::as_slice)))
        .collect()
}

/// Whether the windows read of `file` read it: any window inside a file
/// that fits one read, else windows covering every byte.
fn read_whole(file: &RepoFile, windows: Option<&[(u64, u64)]>) -> bool {
    let Some(windows) = windows else {
        return false;
    };
    if file.size <= READ_WINDOW_BYTES {
        return windows.iter().any(|(start, _)| *start < file.size);
    }
    let mut windows = windows.to_vec();
    windows.sort_unstable();
    let mut covered = 0u64;
    for (start, end) in windows {
        if start > covered {
            return false;
        }
        covered = covered.max(end);
        if covered >= file.size {
            return true;
        }
    }
    false
}

/// A `read_file` call's `offset` or `limit` as the tool reads it: absent or
/// `null` is 0 (for `limit`, the whole window); an integer or a string of
/// digits is that many bytes; anything else was refused by the tool.
fn read_number(value: Option<&serde_json::Value>) -> Option<u64> {
    match value {
        None | Some(serde_json::Value::Null) => Some(0),
        Some(serde_json::Value::Number(number)) => number.as_u64(),
        Some(serde_json::Value::String(text)) => text.trim().parse().ok(),
        Some(_) => None,
    }
}

/// `raw`, a path relative to the repository or absolute inside it, as a
/// normalized repository-relative path (`""` for its root); `None` when it
/// is empty or leaves the repository.
pub fn repo_relative(raw: &str, repo: &Path) -> Option<String> {
    if raw.is_empty() {
        return None;
    }
    let candidate = Path::new(raw);
    let relative = if candidate.is_absolute() {
        candidate.strip_prefix(repo).ok()?
    } else {
        candidate
    };
    let mut parts: Vec<&str> = Vec::new();
    for component in relative.components() {
        match component {
            std::path::Component::Normal(part) => parts.push(part.to_str()?),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                parts.pop()?;
            }
            _ => return None,
        }
    }
    Some(parts.join("/"))
}

#[cfg(test)]
#[path = "audit_files_tests.rs"]
mod tests;
