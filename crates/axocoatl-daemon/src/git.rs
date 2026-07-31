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
    /// Lines added, when known.
    ///
    /// Porcelain status reports *that* a file changed, never how much. A
    /// reviewer deciding what to look at first needs the size of the change,
    /// and "3 files" tells them nothing about whether that is a typo fix or a
    /// rewrite. Filled from `--numstat`; `None` for binaries and for files git
    /// cannot diff, which is different from zero.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub added: Option<u32>,
    /// Lines removed, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub removed: Option<u32>,
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
    /// Agent this lane runs — its prompt, tools and memory, not just its model.
    /// `None` uses the session's agent.
    ///
    /// This is what turns a variants run from "the same agent with different
    /// models" into "different agents on the same task". Designing an agent by
    /// reasoning about it is guesswork; running three and letting the project's
    /// own checks decide is evidence.
    #[serde(default)]
    pub agent: Option<String>,
}

/// One parallel exploration: a `git worktree` on its own branch where a
/// variant agent runs, isolated from the other variants and from the
/// session's primary checkout.
// Deserialize as well as Serialize: the roster is written to disk beside the
// worktrees and read back, so a reload can still say what each lane is.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
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
    /// Agent this lane ran. Present whenever lanes differ by agent, so a
    /// scoreboard can name the thing being compared.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
}

/// How one lane fared against the project's own check command.
///
/// This is the *fan-in* half of a variants run: N candidates are generated in
/// parallel, then each is judged by the repository's real checks (tests, build,
/// typecheck) so the failures are eliminated before a human reads a single diff.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LaneVerdict {
    /// 0-based lane index, matching [`Variant::index`].
    pub index: usize,
    /// True when the check command exited 0 — the lane survives.
    pub passed: bool,
    pub exit_code: i32,
    /// Tail of the check's combined output (capped by [`VERDICT_OUTPUT_MAX`]),
    /// so a failing lane can explain itself without shipping a whole test log.
    pub output: String,
    /// How many files this lane changed.
    ///
    /// Zero is the important case: a lane that changed nothing passes every
    /// check trivially, so `passed` alone would present it as a success. It
    /// happens for real — a model whose tool calls the runtime cannot parse
    /// completes a turn, burns tokens, and edits nothing, with no error
    /// anywhere. Carrying the count lets that be shown for what it is.
    #[serde(default)]
    pub changed_files: usize,
    /// Test files this lane changed.
    ///
    /// A passing check only means something if the tests were an *independent*
    /// arbiter. A lane that rewrote them graded its own work, and "passed" would
    /// be actively misleading — so this is reported rather than folded into
    /// `passed`, and the decision is left to a human who can see it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub touched_tests: Vec<String>,
}

/// Whether a repo path looks like a test the check command would run.
///
/// A heuristic, deliberately broad: false positives merely prompt a human to
/// look, while a false negative would let a lane quietly mark its own homework.
pub fn looks_like_test(path: &str) -> bool {
    let p = path.to_lowercase();
    let file = p.rsplit('/').next().unwrap_or(&p);
    p.split('/').any(|seg| {
        matches!(
            seg,
            "test" | "tests" | "__tests__" | "spec" | "specs" | "e2e"
        )
    }) || file.contains(".test.")
        || file.contains("_test.")
        || file.contains(".spec.")
        || file.contains("_spec.")
        || file.starts_with("test_")
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

/// Whether a model can actually drive a lane.
///
/// A lane model must return **structured** tool calls. Some models emit a
/// perfectly correct call as ordinary text instead, which the runtime never
/// sees — so the agent reads the prompt, answers in a few tokens, edits nothing,
/// and the run reports success because there was nothing to check. Establishing
/// this in one cheap call beats discovering it after a lane has burned minutes
/// producing an empty diff.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelProbe {
    pub model: String,
    /// True when the model returned a tool call in the structured field.
    pub usable: bool,
    /// What happened, in a sentence a user can act on.
    pub detail: String,
}

/// One file the plan expects to change, and what to do in it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PlanStep {
    pub path: String,
    /// The change, specific enough to act on without re-deriving intent.
    pub change: String,
}

/// A task turned into a spec precise enough for a weak model to execute.
///
/// The observed failure mode is not incapacity — it is ambiguity. Given a bare
/// task, a small model produced nothing at all, or syntactically broken code,
/// where a larger one inferred the missing detail and succeeded. The plan is
/// that inference, written down once by an expensive model so several cheap ones
/// can execute it: which files, what change, what not to touch, and how you know
/// it is done.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Plan {
    /// The intent in a sentence.
    pub summary: String,
    pub steps: Vec<PlanStep>,
    /// What must NOT change — the guardrails a weak model otherwise wanders past.
    #[serde(default)]
    pub constraints: Vec<String>,
    /// Observable conditions that mean the work is finished.
    #[serde(default)]
    pub acceptance: Vec<String>,
}

impl Plan {
    /// Render the plan as the instruction each lane receives.
    ///
    /// Deliberately plain imperative prose rather than JSON: this is read by the
    /// executing model, and the prompts that actually worked in testing looked
    /// like this — name the file, name the change, name what not to touch.
    pub fn render(&self, task: &str) -> String {
        let mut s = format!("{task}\n\nPlan:\n{}\n", self.summary);
        if !self.steps.is_empty() {
            s.push_str("\nMake these changes:\n");
            for step in &self.steps {
                s.push_str(&format!("- In {}: {}\n", step.path, step.change));
            }
        }
        if !self.constraints.is_empty() {
            s.push_str("\nDo not:\n");
            for c in &self.constraints {
                s.push_str(&format!("- {c}\n"));
            }
        }
        if !self.acceptance.is_empty() {
            s.push_str("\nDone when:\n");
            for a in &self.acceptance {
                s.push_str(&format!("- {a}\n"));
            }
        }
        s
    }
}

/// What one lane spent.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LaneUsage {
    pub index: usize,
    /// Model this lane ran; `None` means the agent's configured default.
    pub model: Option<String>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// What those tokens cost at this model's price. Local models are 0.
    pub cost_usd: f64,
    /// Wall-clock the lane's agent ran for, measured in the daemon.
    ///
    /// Timed here rather than in the viewer so a reload mid-run does not lose
    /// it, and so the number is the lane's actual work rather than the gap
    /// between two frames arriving over a socket.
    #[serde(default)]
    pub duration_ms: u64,
}

/// The economics of a variants run: what it cost, against what the same work
/// would have cost run entirely on one expensive model.
///
/// This is the argument the product rests on, so it is computed from real token
/// counts rather than estimated. The counterfactual is deliberately generous to
/// the alternative — it prices *the same* token volume at the baseline model's
/// rate, without assuming the frontier model would have needed several attempts.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RunCost {
    pub lanes: Vec<LaneUsage>,
    /// Sum of what the lanes actually cost.
    pub total_usd: f64,
    /// The model the comparison is drawn against.
    pub baseline_model: String,
    /// What the same tokens would have cost on `baseline_model`.
    pub baseline_usd: f64,
    /// `baseline_usd - total_usd`, floored at 0.
    pub saved_usd: f64,
    /// True when every lane priced at 0 — i.e. the run was entirely local.
    pub all_local: bool,
}

/// Price of a model, in dollars per million tokens.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Default)]
pub struct ModelPrice {
    pub input_per_mtok: f64,
    pub output_per_mtok: f64,
}

impl ModelPrice {
    /// Cost of a token count at this price.
    pub fn cost(&self, input_tokens: u64, output_tokens: u64) -> f64 {
        (input_tokens as f64 / 1_000_000.0) * self.input_per_mtok
            + (output_tokens as f64 / 1_000_000.0) * self.output_per_mtok
    }
}

/// One candidate's assessment in a [`Judgment`].
///
/// The useful output of judging N attempts is not a score — it is *why you would
/// pick this one*. A rank with no reasoning just moves the reading problem
/// around; the trade-off is the thing a reviewer actually needs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CandidateRationale {
    /// Lane index this assesses.
    pub index: usize,
    /// 1 = best.
    pub rank: usize,
    /// What this candidate did, in a sentence.
    pub approach: String,
    /// Why you would choose it — or wouldn't.
    pub tradeoffs: String,
}

/// A ranking over the candidates that survived verification.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Judgment {
    /// Lane index of the recommended candidate.
    pub winner: usize,
    /// Every candidate assessed, best first.
    pub candidates: Vec<CandidateRationale>,
    /// The comparison in prose — what actually separates them.
    pub reasoning: String,
}

/// Everything a scoreboard needs about one comparison, in a single read.
///
/// A variants run outlives the page that started it: worktrees persist, checks
/// take minutes, and reloading a tab mid-run is ordinary. Without this the
/// comparison would come back as an empty table even though every result still
/// exists — so the daemon holds the answers and hands them back on request,
/// rather than the browser being the only place they were ever assembled.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct RunResults {
    /// The lanes, with the model or agent each one ran.
    pub lanes: Vec<Variant>,
    /// Verdicts from the last `verify`, empty if it has not been run.
    pub verdicts: Vec<LaneVerdict>,
    /// Token spend and wall-clock per lane, for lanes that have finished.
    pub usage: Vec<LaneUsage>,
    /// The last ranking, if a judge has been run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub judgment: Option<Judgment>,
}

/// Largest patch carried into the judge prompt, per lane. Enough for a real
/// change; short of pasting a refactor of the whole tree into the context.
pub const JUDGE_PATCH_MAX: usize = 24 * 1024;

/// Strip a ``` fence if a model wrapped its JSON in one, so the payload parses.
pub fn unfence_json(s: &str) -> &str {
    let t = s.trim();
    let Some(rest) = t.strip_prefix("```") else {
        return t;
    };
    // Drop an optional language tag on the opening fence.
    let rest = rest.strip_prefix("json").unwrap_or(rest);
    rest.trim_start()
        .strip_suffix("```")
        .map(str::trim)
        .unwrap_or_else(|| rest.trim())
}

/// A variant plus the working-tree status of its worktree — what the Compare
/// lanes show as each variant's changes.
#[derive(Debug, Clone, Serialize)]
pub struct VariantStatus {
    pub index: usize,
    pub branch: String,
    pub worktree: String,
    pub status: GitStatus,
    /// Model this lane ran, recovered from the persisted roster.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Agent this lane ran, recovered from the persisted roster.
    ///
    /// Carried here because a lane's *identity* has to outlive the response
    /// that created it. The worktrees survive a reload and a daemon restart; if
    /// the labels did not, a comparison would come back as "lane 0 vs lane 1"
    /// and stop being a comparison of anything.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
}

/// Merge `git diff --numstat` output into a status, by path.
///
/// numstat reports `adds\tdels\tpath`, with `-` for binary files — which is
/// why the counts are optional rather than defaulting to zero: "we cannot count
/// this" and "nothing changed" are different facts and a reviewer reads them
/// differently.
pub fn apply_numstat(status: &mut GitStatus, numstat: &str) {
    for line in numstat.lines() {
        let mut parts = line.split('\t');
        let (Some(a), Some(d), Some(path)) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        // Renames read `old => new` inside the path field; match on the new one.
        let path = path
            .rsplit(" => ")
            .next()
            .unwrap_or(path)
            .trim_end_matches('}');
        if let Some(f) = status.files.iter_mut().find(|f| f.path == path) {
            f.added = a.parse().ok();
            f.removed = d.parse().ok();
        }
    }
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
            // Counts come from a separate numstat pass; porcelain has none.
            added: None,
            removed: None,
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
                state: "modified".into(),
                added: None,
                removed: None,
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
                state: "renamed".into(),
                added: None,
                removed: None,
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
    fn lane_config_parses_agents_as_well_as_models() {
        // The three comparison modes the API has to express: same agent with
        // different models, different agents entirely, and a mix.
        let lanes: Vec<LaneConfig> = serde_json::from_str(
            r#"[{"model":"qwen3-coder:30b"},{"agent":"careful"},{"agent":"fast","model":"qwen3:8b"},{}]"#,
        )
        .expect("lane configs parse");
        assert_eq!(lanes[0].model.as_deref(), Some("qwen3-coder:30b"));
        assert_eq!(lanes[0].agent, None);
        assert_eq!(lanes[1].agent.as_deref(), Some("careful"));
        assert_eq!(lanes[1].model, None, "an agent lane keeps its own model");
        assert_eq!(lanes[2].agent.as_deref(), Some("fast"));
        assert_eq!(lanes[2].model.as_deref(), Some("qwen3:8b"));
        assert!(lanes[3].agent.is_none() && lanes[3].model.is_none());
    }

    #[test]
    fn variant_labels_the_agent_that_produced_it() {
        let v = Variant {
            index: 0,
            branch: "axo/variant-0".into(),
            worktree: "/w/.axo-variants/0".into(),
            model: None,
            agent: Some("careful".into()),
        };
        let j = serde_json::to_string(&v).unwrap();
        assert!(
            j.contains("careful"),
            "a scoreboard must be able to name it"
        );
        assert!(
            !j.contains("model"),
            "an unset model still stays off the wire"
        );
    }

    #[test]
    fn numstat_sizes_the_changes_and_admits_when_it_cannot() {
        let mut st = GitStatus {
            branch: "main".into(),
            clean: false,
            files: vec![
                GitFile {
                    path: "lib/a.ts".into(),
                    state: "modified".into(),
                    added: None,
                    removed: None,
                },
                GitFile {
                    path: "img/logo.png".into(),
                    state: "modified".into(),
                    added: None,
                    removed: None,
                },
                GitFile {
                    path: "new.txt".into(),
                    state: "untracked".into(),
                    added: None,
                    removed: None,
                },
            ],
        };
        // `-` is git's way of saying a file cannot be counted, not that it is
        // unchanged — the two must not collapse into the same reading.
        apply_numstat(&mut st, "12\t3\tlib/a.ts\n-\t-\timg/logo.png\n");
        assert_eq!(st.files[0].added, Some(12));
        assert_eq!(st.files[0].removed, Some(3));
        assert_eq!(
            st.files[1].added, None,
            "a binary reports no count, not zero"
        );
        assert_eq!(
            st.files[2].added, None,
            "untracked files are absent from numstat"
        );
    }

    #[test]
    fn numstat_follows_a_rename_to_its_new_path() {
        let mut st = GitStatus {
            branch: "main".into(),
            clean: false,
            files: vec![GitFile {
                path: "lib/new.ts".into(),
                state: "renamed".into(),
                added: None,
                removed: None,
            }],
        };
        apply_numstat(&mut st, "4\t1\tlib/old.ts => lib/new.ts\n");
        assert_eq!(st.files[0].added, Some(4));
    }

    #[test]
    fn variant_roster_round_trips_through_disk() {
        // The roster is written beside the worktrees and read back by a later
        // process, so what a lane *is* has to survive the trip. Losing it turns
        // a comparison of two agents into "lane 0 vs lane 1".
        let roster = vec![
            Variant {
                index: 0,
                branch: "axo/variant-0".into(),
                worktree: "/w/.axo-variants/0".into(),
                model: None,
                agent: Some("careful".into()),
            },
            Variant {
                index: 1,
                branch: "axo/variant-1".into(),
                worktree: "/w/.axo-variants/1".into(),
                model: Some("qwen3:8b".into()),
                agent: None,
            },
        ];
        let back: Vec<Variant> =
            serde_json::from_str(&serde_json::to_string(&roster).unwrap()).unwrap();
        assert_eq!(back, roster);
    }

    #[test]
    fn run_results_round_trip_keeps_every_axis_the_board_compares() {
        // One read has to carry identity, outcome, spend and ranking: the
        // scoreboard renders all four, and a reload rebuilds it from this alone.
        let results = RunResults {
            lanes: vec![Variant {
                index: 0,
                branch: "axo/variant-0".into(),
                worktree: "/w/.axo-variants/0".into(),
                model: None,
                agent: Some("careful".into()),
            }],
            verdicts: vec![LaneVerdict {
                index: 0,
                passed: true,
                exit_code: 0,
                output: "ok".into(),
                changed_files: 2,
                touched_tests: vec![],
            }],
            usage: vec![LaneUsage {
                index: 0,
                model: None,
                input_tokens: 100,
                output_tokens: 20,
                cost_usd: 0.0,
                duration_ms: 4_200,
            }],
            judgment: Some(Judgment {
                winner: 0,
                candidates: vec![],
                reasoning: "only survivor".into(),
            }),
        };
        let json = serde_json::to_string(&results).unwrap();
        let back: RunResults = serde_json::from_str(&json).unwrap();
        assert_eq!(back.lanes[0].agent.as_deref(), Some("careful"));
        assert!(back.verdicts[0].passed);
        assert_eq!(back.usage[0].duration_ms, 4_200);
        assert_eq!(back.judgment.unwrap().winner, 0);
    }

    #[test]
    fn lane_usage_tolerates_a_record_written_before_durations_existed() {
        // Usage files persist across upgrades; an older one must still price,
        // reporting an unknown duration rather than failing the whole read.
        let old: LaneUsage = serde_json::from_str(
            r#"{"index":0,"model":null,"input_tokens":10,"output_tokens":2,"cost_usd":0.0}"#,
        )
        .expect("a record without duration_ms still parses");
        assert_eq!(old.duration_ms, 0);
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
    fn recognises_the_test_files_a_lane_must_not_grade_itself_with() {
        for p in [
            "lib/orders.test.ts",
            "src/foo_test.go",
            "tests/integration.rs",
            "__tests__/App.tsx",
            "spec/models/order_spec.rb",
            "e2e/checkout.ts",
            "test_math.py",
        ] {
            assert!(looks_like_test(p), "{p} should be recognised as a test");
        }
        for p in [
            "lib/orders.ts",
            "src/latest.rs", // contains "test" but is not one
            "src/protest/main.rs",
            "README.md",
        ] {
            assert!(!looks_like_test(p), "{p} should NOT be flagged");
        }
    }

    #[test]
    fn plan_renders_the_instruction_a_weak_model_needs() {
        let plan = Plan {
            summary: "Replace the switch with a lookup table.".into(),
            steps: vec![PlanStep {
                path: "lib/orders.ts".into(),
                change: "rewrite compareBy using a Record<SortKey, …> map".into(),
            }],
            constraints: vec!["modify any test file".into()],
            acceptance: vec!["npm run check passes".into()],
        };
        let out = plan.render("Refactor compareBy.");
        // The task leads; the plan supplies what a small model would otherwise
        // have to infer — the file, the change, the guardrail, the finish line.
        assert!(out.starts_with("Refactor compareBy."));
        assert!(out.contains("In lib/orders.ts: rewrite compareBy"));
        assert!(out.contains("Do not:\n- modify any test file"));
        assert!(out.contains("Done when:\n- npm run check passes"));
    }

    #[test]
    fn plan_render_omits_empty_sections() {
        let bare = Plan {
            summary: "Do the thing.".into(),
            steps: vec![],
            constraints: vec![],
            acceptance: vec![],
        };
        let out = bare.render("Task.");
        assert!(!out.contains("Do not:"));
        assert!(!out.contains("Done when:"));
        assert!(!out.contains("Make these changes:"));
    }

    #[test]
    fn model_price_computes_per_million_tokens() {
        let p = ModelPrice {
            input_per_mtok: 3.0,
            output_per_mtok: 15.0,
        };
        // 1M in + 1M out = 3 + 15.
        assert!((p.cost(1_000_000, 1_000_000) - 18.0).abs() < 1e-9);
        // Sub-million scales linearly.
        assert!((p.cost(500_000, 0) - 1.5).abs() < 1e-9);
        // An unpriced model — the default — is free, which is what local means.
        assert_eq!(ModelPrice::default().cost(9_999_999, 9_999_999), 0.0);
    }

    #[test]
    fn unfence_json_handles_how_models_actually_reply() {
        let want = r#"{"winner":1}"#;
        assert_eq!(unfence_json(want), want, "bare JSON passes through");
        assert_eq!(unfence_json("```json\n{\"winner\":1}\n```"), want);
        assert_eq!(unfence_json("```\n{\"winner\":1}\n```"), want);
        assert_eq!(unfence_json("  \n{\"winner\":1}\n  "), want);
        // An unterminated fence still yields parseable content rather than
        // failing the whole judgment.
        assert_eq!(unfence_json("```json\n{\"winner\":1}"), want);
    }

    #[test]
    fn judgment_round_trips() {
        let j = Judgment {
            winner: 2,
            candidates: vec![CandidateRationale {
                index: 2,
                rank: 1,
                approach: "Extracted a helper and reused it".into(),
                tradeoffs: "Clearer, but adds an indirection".into(),
            }],
            reasoning: "Candidate 2 fits the existing style".into(),
        };
        let parsed: Judgment = serde_json::from_str(&serde_json::to_string(&j).unwrap()).unwrap();
        assert_eq!(parsed, j);
    }

    #[test]
    fn variant_omits_model_when_not_overridden() {
        // Uniform runs stay wire-compatible: no `model` key at all.
        let v = Variant {
            index: 0,
            branch: "axo/variant-0".into(),
            worktree: "/w/.axo-variants/0".into(),
            model: None,
            agent: None,
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
