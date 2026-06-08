//! Git status/diff types and parsers for directory sessions.
//!
//! A session is (optionally auto-) a git repo; the daemon drives git inside the
//! session's sandbox container (`AxocoatlDaemon::session_git`), and these pure
//! parsers turn git's porcelain output into the shapes the dashboard's git pane
//! renders. Kept here (separate from the daemon impl) so the parsers are unit-
//! testable without a container.

use serde::Serialize;

/// One changed path in the working tree.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct GitFile {
    pub path: String,
    /// `added` | `modified` | `deleted` | `renamed` | `untracked`.
    pub state: String,
}

/// Working-tree status: current branch + changed files.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct GitStatus {
    pub branch: String,
    pub files: Vec<GitFile>,
    pub clean: bool,
}

/// One file's before/after content — fed straight to Monaco's diff editor.
///
/// `binary` / `too_large` are escape hatches: when either is set, `old` and
/// `new` are blanked (the daemon never streams raw bytes or a multi-megabyte
/// blob into the JSON response or Monaco) and the pane shows a sentinel instead
/// of an inline diff.
#[derive(Debug, Clone, Serialize)]
pub struct GitDiff {
    pub path: String,
    pub old: String,
    pub new: String,
    pub binary: bool,
    pub too_large: bool,
}

/// Largest file (either side) the daemon will inline as a diff. Beyond this we
/// report `too_large` rather than shipping the content. 512 KiB.
pub const DIFF_MAX_BYTES: usize = 512 * 1024;

/// Heuristic binary check: a NUL byte in the first 8 KiB. Matches how git
/// itself decides "binary" for diffs, and survives the lossy-UTF-8 decode the
/// sandbox applies to command output (a real NUL stays a NUL).
pub fn looks_binary(s: &str) -> bool {
    s.as_bytes().iter().take(8192).any(|&b| b == 0)
}

/// Branch list + the current branch.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct GitBranches {
    pub current: String,
    pub branches: Vec<String>,
}

/// One parallel exploration: a `git worktree` on its own branch where a
/// variant agent runs, isolated from the other variants and from the
/// session's primary checkout.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Variant {
    /// 0-based lane index.
    pub index: usize,
    /// Branch name — `axo/variant-{index}`.
    pub branch: String,
    /// Absolute worktree path — `{working_dir}/.axo-variants/{index}`.
    pub worktree: String,
}

/// A variant plus the working-tree status of its worktree — what the Compare
/// lanes show as each variant's changes.
#[derive(Debug, Clone, Serialize)]
pub struct VariantStatus {
    pub index: usize,
    pub branch: String,
    pub worktree: String,
    pub status: GitStatus,
}

/// Parse `git status --porcelain=v1 -b --untracked-files=all`.
pub fn parse_status(stdout: &str) -> GitStatus {
    let mut branch = String::new();
    let mut files = Vec::new();
    for line in stdout.lines() {
        if let Some(rest) = line.strip_prefix("## ") {
            // `main`, `main...origin/main [ahead 1]`, `No commits yet on main`,
            // or `HEAD (no branch)`.
            let b = rest
                .split("...")
                .next()
                .unwrap_or(rest)
                .split(" [")
                .next()
                .unwrap_or(rest);
            branch = b
                .trim_start_matches("No commits yet on ")
                .trim()
                .to_string();
            continue;
        }
        if line.len() < 4 {
            continue;
        }
        let xy = &line[..2];
        let mut path = line[3..].to_string();
        let state = if xy == "??" {
            "untracked"
        } else {
            match xy.trim().chars().next().unwrap_or(' ') {
                'A' => "added",
                'D' => "deleted",
                'R' => "renamed",
                _ => "modified",
            }
        };
        // Renamed entries read `R  old -> new`; show the new path.
        if state == "renamed" {
            if let Some(idx) = path.find(" -> ") {
                path = path[idx + 4..].to_string();
            }
        }
        files.push(GitFile {
            path,
            state: state.to_string(),
        });
    }
    let clean = files.is_empty();
    GitStatus {
        branch,
        files,
        clean,
    }
}

/// Build the branch list from `git branch --format=%(refname:short)` plus the
/// current branch from `git rev-parse --abbrev-ref HEAD`.
pub fn parse_branches(current: &str, list: &str) -> GitBranches {
    GitBranches {
        current: current.trim().to_string(),
        branches: list
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_parses_branch_and_states() {
        let out = "## main...origin/main [ahead 1]\n\
                    M  src/lib.rs\n\
                   ?? new.txt\n\
                   A  added.rs\n\
                    D gone.rs\n";
        let s = parse_status(out);
        assert_eq!(s.branch, "main");
        assert!(!s.clean);
        assert_eq!(s.files.len(), 4);
        assert_eq!(
            s.files[0],
            GitFile {
                path: "src/lib.rs".into(),
                state: "modified".into()
            }
        );
        assert_eq!(s.files[1].state, "untracked");
        assert_eq!(s.files[2].state, "added");
        assert_eq!(s.files[3].state, "deleted");
    }

    #[test]
    fn status_handles_no_commits_and_clean() {
        let s = parse_status("## No commits yet on main\n");
        assert_eq!(s.branch, "main");
        assert!(s.clean);
        assert!(s.files.is_empty());
    }

    #[test]
    fn status_rename_takes_new_path() {
        let s = parse_status("## main\nR  old.rs -> new.rs\n");
        assert_eq!(
            s.files[0],
            GitFile {
                path: "new.rs".into(),
                state: "renamed".into()
            }
        );
    }

    #[test]
    fn binary_heuristic() {
        assert!(looks_binary("text\0more"));
        assert!(looks_binary(&format!("{}\0", "a".repeat(8000))));
        assert!(!looks_binary("fn main() {}\nlet x = 1;\n"));
        assert!(!looks_binary(""));
        // A NUL past the 8 KiB scan window is not flagged.
        assert!(!looks_binary(&format!("{}\0", "a".repeat(9000))));
    }

    #[test]
    fn branches_parse() {
        let b = parse_branches("main\n", "main\naxo/variant-0\naxo/variant-1\n");
        assert_eq!(b.current, "main");
        assert_eq!(b.branches, vec!["main", "axo/variant-0", "axo/variant-1"]);
    }
}
