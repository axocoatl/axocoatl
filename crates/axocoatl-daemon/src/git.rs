//! Git status/diff types and parsers for directory sessions.
//!
//! A session is (optionally auto-) a git repo; the daemon drives git inside the
//! session's sandbox container (`AxocoatlDaemon::session_git`), and these pure
//! parsers turn git's porcelain output into the shapes the dashboard's git pane
//! renders. Kept here (separate from the daemon impl) so the parsers are unit-
//! testable without a container.

use serde::{Deserialize, Serialize};

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

/// How one lane of a variants run executes. Lanes are heterogeneous: a plan
/// produced by an expensive model can be executed by several cheaper ones in
/// parallel, and running the *same* task against *different* models is itself a
/// quality strategy (diverse approaches to choose between).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct LaneConfig {
    /// Model this lane runs. `None` uses the agent's configured model.
    #[serde(default)]
    pub model: Option<String>,
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
    /// Model this lane ran, when the lane overrode the agent's default. Carried
    /// on the response so a comparison view can label each candidate with the
    /// model that produced it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// How one lane fared against the project's own check command.
///
/// This is the *fan-in* half of a variants run: N candidates are generated in
/// parallel, then each is judged by the repository's real checks (tests, build,
/// typecheck) so the failures are eliminated before a human reads a single diff.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct LaneVerdict {
    /// 0-based lane index, matching [`Variant::index`].
    pub index: usize,
    /// True when the check command exited 0 — the lane survives.
    pub passed: bool,
    pub exit_code: i32,
    /// Tail of the check's combined output (capped by [`VERDICT_OUTPUT_MAX`]),
    /// so a failing lane can explain itself without shipping a whole test log.
    pub output: String,
}

/// Most check output carried back per lane. Failures are what matter and the
/// interesting part is at the end, so this keeps the tail.
pub const VERDICT_OUTPUT_MAX: usize = 8 * 1024;

/// Keep the last [`VERDICT_OUTPUT_MAX`] bytes of check output, on a char
/// boundary so the result is always valid UTF-8.
pub fn verdict_tail(s: &str) -> String {
    if s.len() <= VERDICT_OUTPUT_MAX {
        return s.to_string();
    }
    let mut cut = s.len() - VERDICT_OUTPUT_MAX;
    while cut < s.len() && !s.is_char_boundary(cut) {
        cut += 1;
    }
    s[cut..].to_string()
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

    #[test]
    fn lane_config_parses_heterogeneous_models() {
        // The shape the API accepts: one plan, executed by several different
        // (typically cheaper) models concurrently — plus a lane that inherits
        // the agent's own model.
        let lanes: Vec<LaneConfig> =
            serde_json::from_str(r#"[{"model":"qwen3-coder"},{"model":"deepseek-v3"},{}]"#)
                .expect("lane configs parse");
        assert_eq!(lanes.len(), 3);
        assert_eq!(lanes[0].model.as_deref(), Some("qwen3-coder"));
        assert_eq!(lanes[1].model.as_deref(), Some("deepseek-v3"));
        assert_eq!(
            lanes[2].model, None,
            "an empty lane inherits the agent's model"
        );
    }

    #[test]
    fn verdict_tail_keeps_the_end_and_stays_utf8() {
        // Short output passes through untouched.
        assert_eq!(verdict_tail("all tests passed"), "all tests passed");

        // Long output keeps the TAIL — a failing suite's useful part is the end.
        let long = format!("{}FAILED: 3 tests", "x".repeat(VERDICT_OUTPUT_MAX));
        let tail = verdict_tail(&long);
        assert!(tail.len() <= VERDICT_OUTPUT_MAX);
        assert!(tail.ends_with("FAILED: 3 tests"));

        // Truncating mid-multibyte-char must not panic or produce invalid UTF-8.
        let multi = "é".repeat(VERDICT_OUTPUT_MAX);
        let tail = verdict_tail(&multi);
        assert!(tail.chars().all(|c| c == 'é'));
    }

    #[test]
    fn variant_omits_model_when_not_overridden() {
        // Uniform runs stay wire-compatible: no `model` key at all.
        let v = Variant {
            index: 0,
            branch: "axo/variant-0".into(),
            worktree: "/w/.axo-variants/0".into(),
            model: None,
        };
        let json = serde_json::to_string(&v).unwrap();
        assert!(
            !json.contains("model"),
            "unset model must not serialize: {json}"
        );

        let labelled = Variant {
            model: Some("qwen3-coder".into()),
            ..v
        };
        assert!(serde_json::to_string(&labelled)
            .unwrap()
            .contains("qwen3-coder"));
    }
}
