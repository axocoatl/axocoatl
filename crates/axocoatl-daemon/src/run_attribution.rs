//! Which repository paths a run's Agents changed, from the repository
//! captures recorded around each of their activations (Before and After of
//! each, and the captures around each required check). Keep as PR commits
//! exactly these paths; a receipt covers them ([`receipt_files`]).
//!
//! A capture manifest has one line per path: `<base64 path>\t<mode>\t<kind>\t<sha256>`,
//! where the digest is `sha256sum` of a file's content, or of a symbolic
//! link's target as `readlink -n` prints it (`execution_content_repository_snapshot.sh`).
//! Captures larger than their recorded bound keep only a prefix, and nothing
//! is read from an incomplete one.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use axocoatl_session::execution_content::{
    ActivationRepositorySnapshot, ExecutionTurnView, RepositorySnapshotPhase, SPECIAL_KIND,
    UNREAD_DIGEST,
};
use axocoatl_session::receipt::{self, sha256_hex, FileFacts, PathState, Subject};
use axocoatl_session::run_outcome::CheckResult;

/// The reason a repository capture records for an activation whose profile
/// has neither a shell nor a limited write scope (`capture_tool` is `None`).
const NO_CAPTURE_REASON: &str = "does not permit repository capture";

/// The repository paths a run's Agents changed, from the repository
/// captures recorded around each of their activations.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunAttribution {
    /// Repository-relative paths (Git's spelling).
    pub paths: BTreeSet<String>,
    /// Activations whose captures exist but cannot establish what they
    /// changed. Keep refuses rather than guess.
    pub unattributable: Vec<String>,
    /// Facts Keep reports with its result.
    pub notes: Vec<String>,
    /// The trees the run's required checks ran on: each check capture
    /// (`BeforeCheck`/`AfterCheck`) of the run's activations, in order.
    pub checked: Vec<CapturedTree>,
    /// The After capture of the last activation that changed a file, when
    /// one did and no activation whose changes are unknown came after it.
    pub last_writer_after: Option<CapturedTree>,
    /// The nested Git checkouts (not submodules) in the captures of the
    /// run's activations, whose files no capture reads: what an Agent
    /// changed inside one is not known. Keep refuses rather than guess, and
    /// a receipt names each as not covered.
    pub unread: BTreeSet<String>,
    /// Activations whose captures establish what they changed, perhaps
    /// nothing: each could change the repository, and ran to its After
    /// capture.
    pub captured: usize,
}

/// One repository capture's tree: its digest; when the capture holds it
/// whole, its manifest (`<base64 path>\t<mode>\t<kind>\t<sha256>` lines);
/// and the lines of every path that differs from `HEAD`, which a capture
/// records whatever the repository's size.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturedTree {
    pub tree_sha256: String,
    pub manifest: Option<String>,
    pub changed: Option<String>,
}

/// What a capture recorded of its tree that a receipt reads digests from.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Recorded {
    /// The whole manifest: a path it does not hold is absent.
    Whole(BTreeMap<String, ManifestEntry>),
    /// The lines of the paths that differ from `HEAD`: a path they do not
    /// hold is as it is in `HEAD`.
    Changed(BTreeMap<String, ManifestEntry>),
}

impl CapturedTree {
    /// The manifest, when it is complete and its SHA-256 is the tree digest
    /// (the capture's own check, repeated here).
    pub fn verified_manifest(&self) -> Option<&str> {
        self.manifest
            .as_deref()
            .filter(|manifest| sha256_hex(manifest.as_bytes()) == self.tree_sha256)
    }

    /// The whole manifest when the capture held it, else the lines of the
    /// paths that differ from `HEAD`; `None` when it recorded neither.
    fn recorded(&self) -> Option<Recorded> {
        if let Some(manifest) = self.verified_manifest() {
            return parse_manifest(manifest).ok().map(Recorded::Whole);
        }
        self.changed
            .as_deref()
            .and_then(|lines| parse_manifest(lines).ok())
            .map(Recorded::Changed)
    }
}

impl RunAttribution {
    /// Add every activation of one native turn.
    pub fn add_execution_turn(&mut self, turn: &ExecutionTurnView) {
        self.add_turn(turn, None);
    }

    /// A historical receipt must name every executed activation whose effect
    /// cannot be established. A missing capture is not proof of a read-only
    /// profile (an unrestricted file-tool writer can also lack captures).
    pub fn add_native_receipt_turn(
        &mut self,
        turn: &ExecutionTurnView,
        read_only_definitions: &BTreeSet<String>,
    ) {
        self.add_turn(turn, Some(read_only_definitions));
    }

    fn add_turn(
        &mut self,
        turn: &ExecutionTurnView,
        read_only_definitions: Option<&BTreeSet<String>>,
    ) {
        for activation in &turn.activations {
            let reference = &activation.activation.activation;
            let label = format!(
                "{} (generation {}) in turn {}",
                reference.node_id.as_str(),
                reference.generation,
                turn.turn_id.as_str()
            );
            let captures: Vec<CaptureFacts> = activation
                .repository_snapshots
                .iter()
                .map(|view| CaptureFacts::from_snapshot(&view.content))
                .collect();
            if read_only_definitions.is_some_and(|definitions| {
                !definitions.contains(activation.activation.input.definition.snapshot.as_str())
            }) && activation.activation.state
                != axocoatl_session::turn_contract::ActivationState::Unstarted
            {
                self.add_activation_requiring_capture(&label, &captures);
            } else {
                self.add_activation(&label, &captures);
            }
        }
    }

    fn add_activation_requiring_capture(&mut self, label: &str, captures: &[CaptureFacts]) {
        self.add_activation(label, captures);
        if matches!(
            attribute_activation(captures),
            ActivationChanges::NotCaptured
        ) {
            self.last_writer_after = None;
            self.unattributable.push(format!("{label}: repository Before/After captures are unavailable; its file effects cannot be established."));
        }
    }

    pub(crate) fn add_activation(&mut self, label: &str, captures: &[CaptureFacts]) {
        for capture in captures {
            if capture.phase == CapturePhase::Check && capture.usable() {
                if let Some(tree) = capture.tree() {
                    self.checked.push(tree);
                }
            }
            self.unread.extend(capture.unread.iter().cloned());
        }
        match attribute_activation(captures) {
            ActivationChanges::Changed(paths) => {
                self.captured += 1;
                if !paths.is_empty() {
                    self.last_writer_after = captures
                        .iter()
                        .find(|capture| capture.phase == CapturePhase::After)
                        .and_then(CaptureFacts::tree);
                }
                self.paths.extend(paths)
            }
            ActivationChanges::NotCaptured => {}
            ActivationChanges::Unknown(reason) => {
                // It may have changed files after the last known writer: that
                // writer's After capture is no longer what the run left.
                self.last_writer_after = None;
                self.unattributable.push(format!("{label}: {reason}"))
            }
        }
    }

    /// The paths Source Control's Last turn marks for these turns, and why
    /// they are not all known when an activation's captures cannot say what
    /// it changed (such an activation is never read as having changed
    /// nothing).
    pub(crate) fn last_turn(self) -> (Vec<String>, Option<String>) {
        let mut unknown = self.unattributable.clone();
        if let Some(note) = unread_note(&self.unread) {
            unknown.push(note);
        }
        let unattributed = (!unknown.is_empty()).then(|| {
            format!(
                "Axocoatl cannot tell which files part of the last turn changed: {}. Review All \
                 changes.",
                unknown.join("; ")
            )
        });
        (self.paths.into_iter().collect(), unattributed)
    }

    /// Add a turn recorded before repository captures: its file-tool writes
    /// (the "Last turn" attribution of that turn).
    pub fn add_legacy_turn(&mut self, turn_id: &str, touched: Vec<String>) {
        self.paths.extend(touched);
        self.notes.push(format!(
            "Turn {turn_id} predates repository captures; only its file-tool writes are attributed to the run."
        ));
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CapturePhase {
    Before,
    After,
    Check,
}

/// What attribution reads of one repository capture.
#[derive(Debug, Clone)]
pub(crate) struct CaptureFacts {
    pub(crate) phase: CapturePhase,
    pub(crate) unavailable: Option<String>,
    pub(crate) tree_sha256: Option<String>,
    pub(crate) judged_sha256: Option<String>,
    pub(crate) compared: Option<(String, Vec<String>)>,
    /// The complete manifest, when it fit the recorded bound.
    pub(crate) manifest: Option<String>,
    /// The complete patch, when it fit the recorded bound.
    pub(crate) patch: Option<Vec<u8>>,
    /// The lines of the paths that differ from `HEAD`, when recorded.
    pub(crate) changed: Option<String>,
    /// The `HEAD` the capture saw; `None` for an unborn one.
    pub(crate) head: Option<String>,
    /// The nested Git checkouts the capture named, whose files it did not
    /// read.
    pub(crate) unread: Vec<String>,
}

impl CaptureFacts {
    pub(crate) fn from_snapshot(snapshot: &ActivationRepositorySnapshot) -> Self {
        use base64::Engine as _;
        let phase = match snapshot.phase {
            _ if snapshot.condition_run.is_some() => CapturePhase::Check,
            RepositorySnapshotPhase::Before => CapturePhase::Before,
            RepositorySnapshotPhase::After => CapturePhase::After,
            RepositorySnapshotPhase::BeforeCheck { .. }
            | RepositorySnapshotPhase::AfterCheck { .. } => CapturePhase::Check,
        };
        let patch = if snapshot.patch_complete {
            snapshot.patch_base64.as_deref().and_then(|encoded| {
                base64::engine::general_purpose::STANDARD
                    .decode(encoded)
                    .ok()
                    .filter(|bytes| bytes.len() as u64 == snapshot.patch_bytes)
            })
        } else {
            None
        };
        Self {
            phase,
            unavailable: snapshot.unavailable.clone(),
            tree_sha256: snapshot.tree_sha256.clone(),
            judged_sha256: snapshot.judged_sha256.clone(),
            compared: snapshot.compared.as_ref().map(|compared| {
                (
                    compared.before_sha256.clone(),
                    compared.changed_paths.clone(),
                )
            }),
            manifest: snapshot
                .manifest_complete
                .then(|| snapshot.manifest_prefix.clone()),
            patch,
            changed: snapshot.changed_manifest.clone(),
            head: snapshot.head.clone(),
            unread: snapshot.unread_paths.clone(),
        }
    }

    pub(crate) fn usable(&self) -> bool {
        self.unavailable.is_none() && self.tree_sha256.is_some()
    }

    /// The tree this capture recorded, when it recorded one.
    fn tree(&self) -> Option<CapturedTree> {
        if !self.usable() {
            return None;
        }
        Some(CapturedTree {
            tree_sha256: self.tree_sha256.clone()?,
            manifest: self.manifest.clone(),
            changed: self.changed.clone(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ActivationChanges {
    Changed(BTreeSet<String>),
    /// No capture was possible: the activation's profile has neither a shell
    /// nor a limited write scope, or it had no repository.
    NotCaptured,
    Unknown(String),
}

pub(crate) fn attribute_activation(captures: &[CaptureFacts]) -> ActivationChanges {
    let own: Vec<&CaptureFacts> = captures
        .iter()
        .filter(|capture| capture.phase != CapturePhase::Check)
        .collect();
    if own.is_empty() {
        return ActivationChanges::NotCaptured;
    }
    if own.iter().all(|capture| {
        capture
            .unavailable
            .as_deref()
            .is_some_and(|reason| reason.contains(NO_CAPTURE_REASON))
    }) {
        return ActivationChanges::NotCaptured;
    }
    let before = own
        .iter()
        .find(|capture| capture.phase == CapturePhase::Before);
    let after = own
        .iter()
        .find(|capture| capture.phase == CapturePhase::After);
    let (before, after) = match (before, after) {
        (None, _) => return ActivationChanges::Unknown("it has no Before capture".into()),
        (_, None) => return ActivationChanges::Unknown("it has no After capture".into()),
        (Some(before), Some(after)) => (before, after),
    };
    for (name, capture) in [("Before", before), ("After", after)] {
        if !capture.usable() {
            // The capture's own reason, without the host's internal layers.
            let mut reason = capture
                .unavailable
                .as_deref()
                .unwrap_or("no tree was recorded");
            while let Some(rest) = reason
                .strip_prefix("Session dispatch: ")
                .or_else(|| reason.strip_prefix("IO error: "))
            {
                reason = rest;
            }
            return ActivationChanges::Unknown(format!(
                "its {name} capture is unavailable ({})",
                reason.trim_end_matches('.')
            ));
        }
    }
    if let Some((compared_before, paths)) = &after.compared {
        return if before.judged_sha256.as_deref() == Some(compared_before.as_str()) {
            ActivationChanges::Changed(paths.iter().cloned().collect())
        } else {
            ActivationChanges::Unknown(
                "its After capture compared with another Before capture".into(),
            )
        };
    }
    if let (Some(before), Some(after)) = (&before.manifest, &after.manifest) {
        return match manifest_changes(before, after) {
            Ok(paths) => ActivationChanges::Changed(paths),
            Err(reason) => ActivationChanges::Unknown(reason),
        };
    }
    if let (Some(before), Some(after)) = (&before.patch, &after.patch) {
        return match patch_changes(before, after) {
            Ok(paths) => ActivationChanges::Changed(paths),
            Err(reason) => ActivationChanges::Unknown(reason),
        };
    }
    // A change whose patch is larger than one capture records, in a
    // repository whose manifest is too: on the same HEAD, a path that differs
    // from it in neither capture is as in HEAD in both, so the paths whose
    // lines differ between the captures' lines of the paths that differ from
    // HEAD are the ones the activation changed.
    if let (Some(before_lines), Some(after_lines)) = (&before.changed, &after.changed) {
        if before.head == after.head {
            return match manifest_changes(before_lines, after_lines) {
                Ok(paths) => ActivationChanges::Changed(paths),
                Err(reason) => ActivationChanges::Unknown(reason),
            };
        }
        return ActivationChanges::Unknown(format!(
            "HEAD moved between its captures, and {}, so they cannot be compared",
            patch_not_whole()
        ));
    }
    // What a capture does not list when more paths differ from HEAD than it
    // lists, or their lines do not fit its output.
    let unlisted = if before.changed.is_none() {
        "Before"
    } else {
        "After"
    };
    ActivationChanges::Unknown(format!(
        "its {unlisted} capture lists none of the paths that differ from HEAD (it lists at most \
         {}, within its output), and {}, so the captures cannot be compared",
        crate::session_dispatch::group_digits(
            axocoatl_session::execution_content::MAX_CHANGED_PATHS as u64
        ),
        patch_not_whole()
    ))
}

/// Why neither the captures' manifests nor their patches compare: one of
/// the patches is not recorded whole, with the limit that decides it. (Only
/// a repository far smaller than any a run changes has a manifest small
/// enough to record whole.)
fn patch_not_whole() -> String {
    format!(
        "a patch is not recorded whole (a capture records at most {} KiB of it, within its \
         output)",
        axocoatl_session::execution_content::MAX_RECORDED_PATCH_BYTES / 1024
    )
}

/// What the nested Git checkouts `unread` leave unknown, in words: `None`
/// when there are none.
pub(crate) fn unread_note(unread: &BTreeSet<String>) -> Option<String> {
    if unread.is_empty() {
        return None;
    }
    let names: Vec<&str> = unread.iter().map(String::as_str).collect();
    let (are, them) = if names.len() == 1 {
        ("is a nested Git checkout (not a submodule)", "it")
    } else {
        ("are nested Git checkouts (not submodules)", "them")
    };
    Some(format!(
        "{} {are} whose files Axocoatl's repository captures do not read, so what an Agent \
         changed inside {them} is not known",
        names.join(", ")
    ))
}

/// One entry of a capture manifest: the path as recorded (bytes), its mode,
/// kind (`file`, `link`, `deleted`, or `special` for a submodule or nested
/// Git checkout) and content digest (a submodule's commit, or `unread`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestEntry {
    pub path: Vec<u8>,
    pub mode: String,
    pub kind: String,
    pub digest: String,
}

/// Parse a complete capture manifest (`<base64 path>\t<mode>\t<kind>\t<digest>`
/// lines), keyed by the path as text.
pub fn parse_manifest(manifest: &str) -> Result<BTreeMap<String, ManifestEntry>, String> {
    use base64::Engine as _;
    let mut entries = BTreeMap::new();
    for line in manifest.lines().filter(|line| !line.is_empty()) {
        let mut fields = line.split('\t');
        let (Some(encoded), Some(mode), Some(kind), Some(digest), None) = (
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
        ) else {
            return Err("a capture manifest line is unreadable".to_string());
        };
        let path = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|_| "a capture manifest path is unreadable".to_string())?;
        entries.insert(
            String::from_utf8_lossy(&path).into_owned(),
            ManifestEntry {
                path,
                mode: mode.to_string(),
                kind: kind.to_string(),
                digest: digest.to_string(),
            },
        );
    }
    Ok(entries)
}

/// Paths whose manifest entry (mode, kind, content digest) differs between
/// two complete capture manifests (`<base64 path>\t<mode>\t<kind>\t<digest>`).
fn manifest_changes(before: &str, after: &str) -> Result<BTreeSet<String>, String> {
    let before = parse_manifest(before)?;
    let after = parse_manifest(after)?;
    let mut changed = BTreeSet::new();
    for (path, entry) in &before {
        if after.get(path) != Some(entry) {
            changed.insert(path.clone());
        }
    }
    for path in after.keys() {
        if !before.contains_key(path) {
            changed.insert(path.clone());
        }
    }
    Ok(changed)
}

/// Paths whose part of the patch against `HEAD` differs between two
/// complete capture patches.
fn patch_changes(before: &[u8], after: &[u8]) -> Result<BTreeSet<String>, String> {
    let before = patch_blocks(before)?;
    let after = patch_blocks(after)?;
    let mut changed = BTreeSet::new();
    for (header, block) in &before {
        if after.get(header) != Some(block) {
            changed.extend(block_paths(header, block)?);
        }
    }
    for (header, block) in &after {
        if !before.contains_key(header) {
            changed.extend(block_paths(header, block)?);
        }
    }
    Ok(changed)
}

/// Every path a complete capture patch names, as [`block_paths`] reads
/// each of its blocks; `None` when one cannot be read.
pub(crate) fn patch_paths(patch: &[u8]) -> Option<BTreeSet<String>> {
    let mut paths = BTreeSet::new();
    for (header, block) in patch_blocks(patch).ok()? {
        paths.extend(block_paths(&header, &block).ok()?);
    }
    Some(paths)
}

fn patch_blocks(patch: &[u8]) -> Result<BTreeMap<Vec<u8>, Vec<u8>>, String> {
    let mut blocks: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
    let mut current: Option<(Vec<u8>, Vec<u8>)> = None;
    for line in patch.split_inclusive(|byte| *byte == b'\n') {
        if line.starts_with(b"diff --git ") {
            if let Some((header, block)) = current.take() {
                if blocks.insert(header, block).is_some() {
                    return Err("a capture patch names one file twice".into());
                }
            }
            let header = line.strip_suffix(b"\n").unwrap_or(line).to_vec();
            current = Some((header, line.to_vec()));
        } else {
            match &mut current {
                Some((_, block)) => block.extend_from_slice(line),
                None => return Err("a capture patch is unreadable".into()),
            }
        }
    }
    if let Some((header, block)) = current {
        if blocks.insert(header, block).is_some() {
            return Err("a capture patch names one file twice".into());
        }
    }
    Ok(blocks)
}

/// Undo Git's C-style quoting of a path (`"a/\303\251 x"`), or take it as
/// is. Returns the path and what follows it.
fn unquote_path(text: &[u8]) -> Option<(Vec<u8>, &[u8])> {
    if text.first() != Some(&b'"') {
        return Some((text.to_vec(), &[]));
    }
    let mut out = Vec::new();
    let mut index = 1;
    while index < text.len() {
        match text[index] {
            b'"' => return Some((out, &text[index + 1..])),
            b'\\' => {
                let next = *text.get(index + 1)?;
                index += 2;
                match next {
                    b'n' => out.push(b'\n'),
                    b't' => out.push(b'\t'),
                    b'a' => out.push(7),
                    b'b' => out.push(8),
                    b'f' => out.push(12),
                    b'r' => out.push(b'\r'),
                    b'v' => out.push(11),
                    b'0'..=b'7' => {
                        let digits = text.get(index - 1..index + 2)?;
                        if !digits.iter().all(|digit| (b'0'..=b'7').contains(digit)) {
                            return None;
                        }
                        let value = digits
                            .iter()
                            .fold(0u32, |value, digit| value * 8 + u32::from(digit - b'0'));
                        out.push(u8::try_from(value).ok()?);
                        index += 2;
                    }
                    other => out.push(other),
                }
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    None
}

fn strip_side(path: Vec<u8>, prefix: &[u8]) -> Option<String> {
    let rest = path.strip_prefix(prefix)?;
    (!rest.is_empty()).then(|| String::from_utf8_lossy(rest).into_owned())
}

/// The paths one block of a patch is about: from its extended header
/// (`rename from/to`, `copy from/to`, `---`/`+++`) before any hunk, else from
/// the `diff --git a/P b/P` line.
fn block_paths(header: &[u8], block: &[u8]) -> Result<BTreeSet<String>, String> {
    let unreadable = || "a capture patch header is unreadable".to_string();
    let mut paths = BTreeSet::new();
    for line in block.split(|byte| *byte == b'\n').skip(1) {
        if line.starts_with(b"@@")
            || line.starts_with(b"GIT binary patch")
            || line.starts_with(b"Binary files ")
        {
            break;
        }
        for (prefix, side) in [
            (&b"rename from "[..], &b""[..]),
            (b"rename to ", b""),
            (b"copy from ", b""),
            (b"copy to ", b""),
            (b"--- ", b"a/"),
            (b"+++ ", b"b/"),
        ] {
            let Some(rest) = line.strip_prefix(prefix) else {
                continue;
            };
            if rest == b"/dev/null" {
                continue;
            }
            let rest = rest.strip_suffix(b"\t").unwrap_or(rest);
            let (path, _) = unquote_path(rest).ok_or_else(unreadable)?;
            let path = if side.is_empty() {
                (!path.is_empty()).then(|| String::from_utf8_lossy(&path).into_owned())
            } else {
                strip_side(path, side)
            };
            paths.insert(path.ok_or_else(unreadable)?);
        }
    }
    if !paths.is_empty() {
        return Ok(paths);
    }
    let rest = header.strip_prefix(b"diff --git ").ok_or_else(unreadable)?;
    if rest.first() == Some(&b'"') {
        let (first, after) = unquote_path(rest).ok_or_else(unreadable)?;
        let after = after.strip_prefix(b" ").ok_or_else(unreadable)?;
        let (second, _) = unquote_path(after).ok_or_else(unreadable)?;
        paths.insert(strip_side(first, b"a/").ok_or_else(unreadable)?);
        paths.insert(strip_side(second, b"b/").ok_or_else(unreadable)?);
        return Ok(paths);
    }
    // `a/P b/P`: the two halves name the same path.
    if rest.len() >= 7 && (rest.len() - 5) % 2 == 0 && rest.starts_with(b"a/") {
        let length = (rest.len() - 5) / 2;
        let first = &rest[2..2 + length];
        let middle = &rest[2 + length..5 + length];
        let second = &rest[5 + length..];
        if middle == b" b/" && first == second {
            paths.insert(String::from_utf8_lossy(first).into_owned());
            return Ok(paths);
        }
    }
    Err(unreadable())
}

/// A path inside Git's own directory at any depth.
pub fn is_git_internal(path: &str) -> bool {
    path.split('/')
        .any(|segment| segment.eq_ignore_ascii_case(".git"))
}

/// Where a receipt's subject digests come from, and what is not covered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceiptFiles {
    pub subject: Vec<Subject>,
    pub facts: FileFacts,
}

/// The tree the run's required checks judged: the one tree their results
/// name (`candidate_sha256`), when its capture is among the run's and
/// recorded its whole manifest or the lines of its changed paths. Checks
/// that name no tree, or more than one, judged none that a receipt can name.
fn judged_tree<'a>(
    attribution: &'a RunAttribution,
    checks: &[CheckResult],
) -> Option<&'a CapturedTree> {
    let candidates: BTreeSet<&str> = checks
        .iter()
        .filter_map(|check| check.candidate_sha256.as_deref())
        .collect();
    let [candidate] = candidates.into_iter().collect::<Vec<_>>()[..] else {
        return None;
    };
    attribution
        .checked
        .iter()
        .rev()
        .find(|tree| tree.tree_sha256 == candidate && tree.recorded().is_some())
}

/// The subjects and file facts of a receipt: every path `attribution`
/// names, with its digest from the judged capture (the tree the required
/// checks ran on), else from the last writer's After capture, else as read
/// under `repo_root` now. A capture gives a path's digest from its whole
/// manifest, or, in a repository too large for that, from the lines of the
/// paths that differ from `HEAD`; a path those lines do not hold was as in
/// `HEAD` in that tree, and is read under `repo_root` now and listed in
/// `read_at_finish`. A submodule is a subject with the commit it holds.
/// Removed paths are listed apart; a path that is Git's own, not plain, or
/// not a file, link or submodule, every nested Git checkout in the run's
/// captures (whose files no capture reads), and every activation whose
/// changes cannot be attributed, makes the list incomplete and says why.
pub fn receipt_files(
    attribution: &RunAttribution,
    checks: &[CheckResult],
    repo_root: &Path,
) -> ReceiptFiles {
    receipt_files_with_fallback(attribution, checks, Some(repo_root))
}

/// A historical Session turn can be exported long after the checkout changed.
/// Only retained captures may supply its file hashes; missing capture content
/// is reported as incomplete, never filled from the current working tree.
pub fn receipt_files_from_captures(
    attribution: &RunAttribution,
    checks: &[CheckResult],
) -> ReceiptFiles {
    receipt_files_with_fallback(attribution, checks, None)
}

fn receipt_files_with_fallback(
    attribution: &RunAttribution,
    checks: &[CheckResult],
    repo_root: Option<&Path>,
) -> ReceiptFiles {
    let captured = judged_tree(attribution, checks)
        .map(|tree| (receipt::source::JUDGED_CAPTURE, tree))
        .or_else(|| {
            attribution
                .last_writer_after
                .as_ref()
                .filter(|tree| tree.recorded().is_some())
                .map(|tree| (receipt::source::AFTER_CAPTURE, tree))
        });
    let recorded = captured.and_then(|(_, tree)| tree.recorded());
    let (source, tree_sha256) = match (&captured, &recorded) {
        (Some((source, tree)), Some(_)) => (source.to_string(), Some(tree.tree_sha256.clone())),
        _ if repo_root.is_some() => (receipt::source::WORKSPACE_AT_FINISH.to_string(), None),
        _ => ("not_captured".to_string(), None),
    };
    let mut subject = Vec::new();
    let mut links = Vec::new();
    let mut removed = Vec::new();
    let mut read_at_finish = Vec::new();
    let mut unattributed: Vec<String> = attribution.unattributable.clone();
    let read_current = |path: &str| match repo_root {
        Some(root) => receipt::hash_repo_path(root, path),
        None => PathState::Other("its historical file hash is not retained in a capture".into()),
    };
    let state_of = |entry: &ManifestEntry| {
        if std::str::from_utf8(&entry.path).is_err() {
            return PathState::Other("its name is not UTF-8".into());
        }
        match entry.kind.as_str() {
            "file" => PathState::File(entry.digest.clone()),
            "link" => PathState::Link(entry.digest.clone()),
            "deleted" => PathState::Absent,
            SPECIAL_KIND if entry.digest == UNREAD_DIGEST => {
                PathState::Other(receipt::NESTED_CHECKOUT_NOT_COVERED.into())
            }
            SPECIAL_KIND if receipt::is_commit(&entry.digest) => {
                PathState::Commit(entry.digest.clone())
            }
            kind => PathState::Other(format!("recorded as {kind:?}")),
        }
    };
    for path in &attribution.paths {
        if is_git_internal(path) {
            unattributed.push(format!(
                "{path}: one of Git's own files, which a receipt does not cover"
            ));
            continue;
        }
        if !receipt::is_plain_path(path) {
            unattributed.push(format!("{path:?}: not a plain repository path"));
            continue;
        }
        let state = match &recorded {
            Some(Recorded::Whole(entries)) => entries.get(path).map_or(PathState::Absent, state_of),
            Some(Recorded::Changed(entries)) => match entries.get(path) {
                Some(entry) => state_of(entry),
                None => {
                    if repo_root.is_some() {
                        read_at_finish.push(path.clone());
                    }
                    read_current(path)
                }
            },
            None => read_current(path),
        };
        match state {
            PathState::File(digest) => subject.push(Subject::sha256(path.clone(), digest)),
            PathState::Link(digest) => {
                subject.push(Subject::sha256(path.clone(), digest));
                links.push(path.clone());
            }
            PathState::Commit(commit) => subject.push(Subject::git_commit(path.clone(), commit)),
            PathState::Absent => removed.push(path.clone()),
            PathState::Other(why) => unattributed.push(format!("{path}: {why}")),
        }
    }
    // A nested checkout the run's Agents may have changed inside, though
    // its own entry did not change.
    for path in attribution.unread.difference(&attribution.paths) {
        unattributed.push(format!("{path}: {}", receipt::NESTED_CHECKOUT_NOT_COVERED));
    }
    ReceiptFiles {
        subject,
        facts: FileFacts {
            source,
            tree_sha256,
            links,
            removed,
            read_at_finish,
            complete: unattributed.is_empty(),
            unattributed,
        },
    }
}

#[cfg(test)]
#[path = "run_attribution_tests.rs"]
mod tests;
