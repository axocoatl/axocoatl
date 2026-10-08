//! The host's checks of the findings an audit reports, against the
//! repository as the host listed it ([`super::files::list_repository`]),
//! never against what a model says about it.
//!
//! - **Paths** ([`check_locations`]): a finding's file is looked up in the
//!   host's listing after a leading `./` and the repository's place in
//!   front of it are removed (its absolute path on the host or in the
//!   checkout, or its directory's name). A file the listing does not hold
//!   and the host cannot find in the repository either is not reported as a
//!   finding but as a note, "finding at a path that does not exist: <path>
//!   (<title>)". In the 1.3.0 re-smoke a planner invented
//!   `notify/rotate_keys.py` and a finding at it reached the Outcome.
//! - **Lines**: a finding whose line is beyond the end of its file, as the
//!   host counts the file's lines, keeps its file, and its line becomes
//!   unknown (`null`), with a note.
//! - **Near duplicates** ([`remove_near_duplicates`]): after the
//!   integration (and after the host's own merge), findings at the same
//!   file and the same or a nearby line (at most [`NEAR_LINES`] apart, or
//!   both without a line) whose titles share most of their words
//!   ([`similar_titles`]) are one finding: the most specific is kept
//!   ([`more_specific`]) and a note counts the rest. In the re-smoke the
//!   integrator kept `billing-F3` and `rest-F1`, the same overflow claim at
//!   `billing/pagination.py:8` from two areas.
//!
//! Owner: audit.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path};

use axocoatl_session::run_outcome::Finding;

use super::files::RepoFile;

/// The most lines apart two findings' lines may be to count as one place.
pub const NEAR_LINES: u32 = 2;
/// The most findings a note of one kind names; the rest are counted.
const MAX_NAMED: usize = 16;
/// Words a title's similarity leaves out: they say nothing about which
/// defect it is.
const TITLE_FILLER: &[&str] = &[
    "a",
    "an",
    "the",
    "in",
    "of",
    "on",
    "at",
    "to",
    "for",
    "with",
    "and",
    "or",
    "is",
    "are",
    "be",
    "by",
    "from",
    "via",
    "function",
    "method",
    "potential",
    "possible",
    "issue",
    "bug",
    "vulnerability",
    "risk",
];

/// The repository as the host listed it, for looking a finding's file up.
pub struct Listed<'a> {
    files: BTreeMap<&'a str, &'a RepoFile>,
    repo: &'a Path,
}

impl<'a> Listed<'a> {
    pub fn new(files: &'a [RepoFile], repo: &'a Path) -> Self {
        Self {
            files: files
                .iter()
                .map(|file| (file.path.as_str(), file))
                .collect(),
            repo,
        }
    }

    /// The repository-relative path a finding's file names: the listed path
    /// it is after `./` and the repository's place in front of it are
    /// removed, else such a path the host finds as a regular file in the
    /// repository (one the listing leaves out, such as an ignored file).
    /// `None` when there is none.
    pub fn resolve(&self, written: &str) -> Option<String> {
        let candidates = self.candidates(written);
        if let Some(listed) = candidates
            .iter()
            .find(|candidate| self.files.contains_key(candidate.as_str()))
        {
            return Some(listed.clone());
        }
        candidates.into_iter().find(|candidate| {
            std::fs::symlink_metadata(self.repo.join(candidate))
                .map(|metadata| metadata.is_file())
                .unwrap_or(false)
        })
    }

    /// Repository-relative paths `written` may mean, most literal first.
    fn candidates(&self, written: &str) -> Vec<String> {
        let written = written.trim().trim_matches('`').trim();
        let path = Path::new(written);
        let mut candidates = Vec::new();
        if path.is_absolute() {
            if let Some(relative) = path.strip_prefix(self.repo).ok().and_then(normalized) {
                candidates.push(relative);
            }
            // A checkout elsewhere (inside a container, say): the longest
            // tail of the path that the listing holds.
            if let Some(parts) = normalized(path) {
                let parts: Vec<&str> = parts.split('/').collect();
                for start in 0..parts.len() {
                    let tail = parts[start..].join("/");
                    if self.files.contains_key(tail.as_str()) {
                        candidates.push(tail);
                        break;
                    }
                }
            }
        } else if let Some(relative) = normalized(path) {
            if let Some((first, rest)) = relative.split_once('/') {
                let named = self
                    .repo
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name == first);
                candidates.push(relative.clone());
                if named {
                    candidates.push(rest.to_owned());
                }
            } else {
                candidates.push(relative);
            }
        }
        candidates.retain(|candidate| !candidate.is_empty());
        candidates
    }

    fn file(&self, path: &str) -> Option<&RepoFile> {
        self.files.get(path).copied()
    }
}

/// `path`'s normal components joined by `/`, with `.` left out and `..`
/// taking the previous one away; `None` when `..` leaves it.
fn normalized(path: &Path) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_str()?),
            Component::CurDir | Component::RootDir | Component::Prefix(_) => {}
            Component::ParentDir => {
                parts.pop()?;
            }
        }
    }
    Some(parts.join("/"))
}

/// The file part of a finding's location: what precedes its line when it
/// names one, else the whole location.
pub fn location_file(finding: &Finding) -> Option<&str> {
    let location = finding.location.as_deref()?.trim();
    let file = match finding.line {
        Some(Some(_)) => location.split_once(':').map_or(location, |(file, _)| file),
        _ => location,
    };
    let file = file.trim();
    let file = file.strip_prefix("./").unwrap_or(file);
    (!file.is_empty()).then_some(file)
}

/// How many lines the file at `repo`/`path` has: its newlines, and one
/// more when its last line has none. `None` when the host cannot read it.
pub fn host_line_count(repo: &Path, path: &str) -> Option<u64> {
    use std::io::Read;
    let mut file = std::fs::File::open(repo.join(path)).ok()?;
    let mut buffer = vec![0u8; 64 * 1024];
    let mut newlines = 0u64;
    let mut last = None;
    loop {
        let read = file.read(&mut buffer).ok()?;
        if read == 0 {
            break;
        }
        newlines += buffer[..read].iter().filter(|byte| **byte == b'\n').count() as u64;
        last = Some(buffer[read - 1]);
    }
    Some(match last {
        Some(b'\n') | None => newlines,
        Some(_) => newlines + 1,
    })
}

/// Check each finding's file and line against the host's listing:
/// a finding at a file the repository does not have becomes a note, and a
/// line beyond the end of its file (`line_count` of the resolved path)
/// becomes unknown, with a note. Findings without a location pass as they
/// are. Returns the findings kept and the notes, in finding order.
pub fn check_locations(
    findings: Vec<Finding>,
    listed: &Listed<'_>,
    line_count: impl Fn(&str) -> Option<u64>,
) -> (Vec<Finding>, Vec<String>) {
    let mut kept = Vec::with_capacity(findings.len());
    let mut missing = Vec::new();
    let mut beyond = Vec::new();
    for mut finding in findings {
        let Some(written) = location_file(&finding).map(str::to_owned) else {
            kept.push(finding);
            continue;
        };
        let Some(path) = listed.resolve(&written) else {
            missing.push(format!(
                "finding at a path that does not exist: {written} ({})",
                finding.title
            ));
            continue;
        };
        if path != written {
            // Name the file as the listing does, so findings compare.
            let location = finding.location.as_deref().unwrap_or_default();
            finding.location = Some(match (finding.line, location.split_once(':')) {
                (Some(Some(_)), Some((_, rest))) => format!("{path}:{rest}"),
                _ => path.clone(),
            });
        }
        if let Some(Some(line)) = finding.line {
            let lines = match listed.file(&path) {
                Some(file) if file.size == 0 => Some(0),
                _ => line_count(&path),
            };
            if let Some(lines) = lines.filter(|lines| u64::from(line) > *lines) {
                beyond.push(format!(
                    "finding at line {line} of {path}, which has {lines} line{}: its line is \
                     left unknown ({})",
                    if lines == 1 { "" } else { "s" },
                    finding.title
                ));
                finding.location = Some(path);
                finding.line = Some(None);
            }
        }
        kept.push(finding);
    }
    let mut notes = named(missing, "more findings at paths that do not exist");
    notes.extend(named(beyond, "more findings at lines beyond their files"));
    (kept, notes)
}

/// At most [`MAX_NAMED`] notes, then one counting the rest.
fn named(mut notes: Vec<String>, rest: &str) -> Vec<String> {
    if notes.len() > MAX_NAMED {
        let more = notes.len() - MAX_NAMED;
        notes.truncate(MAX_NAMED);
        notes.push(format!("{more} {rest} (not listed)"));
    }
    notes
}

/// A title's words as the similarity counts them: lowercase runs of
/// letters and digits (`page_count` is `page` and `count`), without
/// [`TITLE_FILLER`].
fn title_words(title: &str) -> BTreeSet<String> {
    title
        .to_lowercase()
        .split(|character: char| !character.is_alphanumeric())
        .filter(|word| !word.is_empty() && !TITLE_FILLER.contains(word))
        .map(str::to_owned)
        .collect()
}

/// Whether two titles name the same defect: their words
/// ([`title_words`]) share more than half of all the words either has.
/// "Integer Overflow in page_count Function" and "Potential Integer
/// Overflow in page_count function" do; "Hardcoded Webhook Token" and
/// "Hardcoded Webhook URL" (two of four words) do not.
pub fn similar_titles(a: &str, b: &str) -> bool {
    let (a, b) = (title_words(a), title_words(b));
    if a.is_empty() || b.is_empty() {
        return false;
    }
    let shared = a.intersection(&b).count();
    let all = a.union(&b).count();
    shared * 2 > all
}

/// The file and line two findings are compared at.
fn place(finding: &Finding) -> Option<(String, Option<u32>)> {
    let file = location_file(finding)?;
    Some((file.to_owned(), finding.line.flatten()))
}

/// Whether `a` says more than `b`: a known line over none, then the longer
/// detail, then a severity over none.
pub fn more_specific(a: &Finding, b: &Finding) -> bool {
    let rank = |finding: &Finding| {
        (
            finding.line.flatten().is_some(),
            finding.detail.len(),
            finding.severity.is_some(),
        )
    };
    rank(a) > rank(b)
}

/// Remove near duplicates: a finding at the same file as an earlier kept
/// one, with both lines unknown or at most [`NEAR_LINES`] apart, and a
/// similar title ([`similar_titles`]). The more specific of the two
/// ([`more_specific`]) stays, in the earlier one's place. Returns the
/// findings kept and a note when any was removed.
pub fn remove_near_duplicates(findings: Vec<Finding>) -> (Vec<Finding>, Option<String>) {
    let mut kept: Vec<Finding> = Vec::with_capacity(findings.len());
    let mut removed: Vec<(String, String)> = Vec::new();
    for finding in findings {
        let same = place(&finding).and_then(|(file, line)| {
            kept.iter().position(|earlier| {
                place(earlier).is_some_and(|(earlier_file, earlier_line)| {
                    earlier_file == file
                        && match (earlier_line, line) {
                            (Some(a), Some(b)) => a.abs_diff(b) <= NEAR_LINES,
                            (None, None) => true,
                            _ => false,
                        }
                        && similar_titles(&earlier.title, &finding.title)
                })
            })
        });
        match same {
            Some(index) => {
                let id = |finding: &Finding| {
                    if finding.id.is_empty() {
                        format!("\"{}\"", finding.title)
                    } else {
                        finding.id.clone()
                    }
                };
                if more_specific(&finding, &kept[index]) {
                    removed.push((id(&kept[index]), id(&finding)));
                    kept[index] = finding;
                } else {
                    removed.push((id(&finding), id(&kept[index])));
                }
            }
            None => kept.push(finding),
        }
    }
    if removed.is_empty() {
        return (kept, None);
    }
    let count = removed.len();
    let mut pairs: Vec<String> = removed
        .iter()
        .take(MAX_NAMED)
        .map(|(gone, stays)| format!("{gone} (kept {stays})"))
        .collect();
    if count > MAX_NAMED {
        pairs.push(format!("{} more", count - MAX_NAMED));
    }
    let note = format!(
        "the host removed {count} finding{} that repeat{} another at the same file and line (at \
         most {NEAR_LINES} lines apart) with a similar title, keeping the more specific: {}",
        if count == 1 { "" } else { "s" },
        if count == 1 { "s" } else { "" },
        pairs.join(", ")
    );
    (kept, Some(note))
}

#[cfg(test)]
#[path = "audit_findings_tests.rs"]
mod tests;
