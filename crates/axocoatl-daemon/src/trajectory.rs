//! Trajectory alignment — comparing *how* lanes worked, not just what they made.
//!
//! A scoreboard says which candidates passed and what they cost. It cannot say
//! anything about the interesting case: two lanes that both pass. When the
//! scores tie, the only thing left to judge is the route each one took — did it
//! read the file before editing it, did it flail through six greps, did it reach
//! for the shell when a targeted edit would do.
//!
//! Raw tool logs cannot answer that either, because they are not comparable.
//! Two lanes doing the same work emit different tool names, different argument
//! shapes and different orderings, so a reader ends up diffing noise. This
//! module does three things to make the comparison real:
//!
//! 1. **Normalise** every tool call into a small canonical taxonomy of *acts* —
//!    what was done, and to what.
//! 2. **Align** the lanes against a baseline, so equivalent steps sit on the
//!    same row even when they happened at different points in each run.
//! 3. **Mark agreement**, so a viewer can collapse the stretches where the lanes
//!    did the same thing and spend its attention on where they parted.
//!
//! The alignment is deliberately a *star* alignment against one baseline lane
//! rather than a full multiple-sequence alignment. Optimal N-way alignment is
//! exponential, and the extra fidelity would be spent on a question nobody
//! asked: the user already chose a baseline in the scoreboard, and "how does
//! each of these differ from the one I'd have shipped" is the actual question.

use serde::{Deserialize, Serialize};

/// The canonical taxonomy: what kind of act a tool call was.
///
/// Small on purpose. The point is to make two lanes comparable, and a taxonomy
/// with one arm per tool would just reproduce the tool names it was meant to
/// abstract over. Anything unrecognised — an MCP tool, a plugin — lands in
/// `Other` and still aligns by name, so nothing is silently dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionKind {
    /// Looked at a specific file.
    Read,
    /// Changed part of an existing file.
    Edit,
    /// Replaced a file wholesale, or created one.
    Write,
    /// Enumerated a directory.
    List,
    /// Searched by content or by name.
    Search,
    /// Ran a command.
    Run,
    /// Something else — kept, named, and aligned by tool name.
    Other,
}

impl ActionKind {
    /// Does *what* this act carried matter, or only that it happened?
    ///
    /// This single question decides the whole comparison's signal-to-noise
    /// ratio. Observation is fungible: reading the same file at a different
    /// offset, or grepping it with a slightly different pattern, is the same
    /// act of looking, and treating those as divergence buries the real finding
    /// under trivia. Mutation is not fungible — *what* you wrote to a file is
    /// the substance of what you did, so two lanes that both edit one file have
    /// genuinely diverged unless the edit itself matches.
    fn payload_is_substance(self) -> bool {
        matches!(self, ActionKind::Edit | ActionKind::Write | ActionKind::Run)
    }
}

/// One normalised step in a lane's trajectory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Action {
    /// Position within its own lane, 0-based — what "step 4" means.
    pub seq: usize,
    pub kind: ActionKind,
    /// The raw tool name. Normalising must never mean hiding: a reader has to be
    /// able to see that "Read" was `read_file` and not something exotic.
    pub tool: String,
    /// What was acted on — a path, a pattern, or the head of a command.
    pub target: String,
    /// The substance, for acts where the payload is the point. An edit's
    /// old→new, or the full command. `None` for pure observation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// The tool reported an error.
    #[serde(default)]
    pub failed: bool,
}

/// Longest a `detail` is kept. Enough to see what an edit actually did without
/// carrying a rewritten file into every comparison payload.
const DETAIL_MAX: usize = 240;

fn clip(s: &str) -> String {
    // Char-boundary safe: `s` is arbitrary model output and may be multi-byte.
    if s.chars().count() <= DETAIL_MAX {
        return s.to_string();
    }
    let head: String = s.chars().take(DETAIL_MAX).collect();
    format!("{head}…")
}

/// Collapse whitespace so two lanes that formatted the same command differently
/// still compare equal. Substance, not spacing.
fn squeeze(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Strip a leading working-directory prefix so lanes in different worktrees
/// report the same path.
///
/// Without this every single step diverges: lane 0 edits
/// `…/.axo-variants/0/lib/orders.ts` and lane 1 edits
/// `…/.axo-variants/1/lib/orders.ts`, which are the same file in the comparison
/// that matters and different strings everywhere else.
fn relativize(path: &str) -> String {
    let p = path.trim();
    if let Some(i) = p.find(".axo-variants/") {
        let rest = &p[i + ".axo-variants/".len()..];
        // Drop the lane index segment that follows.
        if let Some(j) = rest.find('/') {
            return rest[j + 1..].to_string();
        }
    }
    p.trim_start_matches("./").to_string()
}

impl Action {
    /// *Where* this step sits in the route: the act and its object, nothing more.
    ///
    /// This is the alignment key, and keeping the payload out of it is the whole
    /// trick. Two lanes that both edit `orders.ts` are at the same point in the
    /// route even when they edit it differently — so they belong on one row,
    /// side by side, which is precisely the comparison worth showing. Fold the
    /// payload in here instead and that row splits into two orphans, each lane
    /// appearing to have done something the other never did.
    pub fn slot(&self) -> String {
        let kind = serde_json::to_string(&self.kind).unwrap_or_default();
        if self.kind == ActionKind::Other {
            // Nothing is known about an unrecognised tool's semantics, so its
            // identity is its name plus its target — never merged with another
            // tool just because both were unrecognised.
            format!("{kind}|{}|{}", self.tool, self.target)
        } else {
            format!("{kind}|{}", self.target)
        }
    }

    /// *What* this step actually did — the slot plus the substance.
    ///
    /// Same slot, different signature, is the definition of a divergence. See
    /// [`ActionKind::payload_is_substance`]: observation carries no substance, so
    /// reading a file at a different offset stays agreement rather than being
    /// reported as a difference nobody cares about.
    pub fn signature(&self) -> String {
        if self.kind.payload_is_substance() {
            format!("{}|{}", self.slot(), self.detail.as_deref().unwrap_or(""))
        } else {
            self.slot()
        }
    }

    /// Normalise one tool call into an act.
    ///
    /// `arguments` is whatever the model produced, so every lookup is defensive:
    /// a malformed call still becomes a step (with an empty target) rather than
    /// vanishing from the trajectory. A step you cannot interpret is still
    /// evidence of what the lane tried.
    pub fn from_call(seq: usize, tool: &str, arguments: Option<&serde_json::Value>) -> Action {
        let arg = |key: &str| -> Option<String> {
            arguments
                .and_then(|a| a.get(key))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        };
        let (kind, target, detail) = match tool {
            "read_file" => (
                ActionKind::Read,
                relativize(&arg("path").unwrap_or_default()),
                None,
            ),
            "write_file" => {
                let body = arg("content").unwrap_or_default();
                (
                    ActionKind::Write,
                    relativize(&arg("path").unwrap_or_default()),
                    Some(format!("wrote {} bytes", body.len())),
                )
            }
            "edit_file" => {
                let old = squeeze(&arg("old").unwrap_or_default());
                let new = squeeze(&arg("new").unwrap_or_default());
                (
                    ActionKind::Edit,
                    relativize(&arg("path").unwrap_or_default()),
                    Some(clip(&format!("{old} → {new}"))),
                )
            }
            "list_dir" => (
                ActionKind::List,
                relativize(&arg("path").unwrap_or_else(|| ".".to_string())),
                None,
            ),
            "grep" => {
                let pat = arg("pattern").unwrap_or_default();
                let scope = arg("path").map(|p| relativize(&p)).unwrap_or_default();
                (
                    ActionKind::Search,
                    if scope.is_empty() {
                        pat.clone()
                    } else {
                        format!("{pat} in {scope}")
                    },
                    None,
                )
            }
            "glob" => (ActionKind::Search, arg("pattern").unwrap_or_default(), None),
            "bash" | "bash_background" | "spawn_terminal" => {
                let cmd = squeeze(&arg("command").unwrap_or_default());
                (
                    ActionKind::Run,
                    // The head of the command names the act; the whole thing is
                    // the substance and lives in `detail`.
                    cmd.split_whitespace()
                        .next()
                        .unwrap_or_default()
                        .to_string(),
                    Some(clip(&cmd)),
                )
            }
            other => {
                // An unknown tool still gets a best-effort target from the keys
                // that conventionally carry one.
                let t = arg("path")
                    .or_else(|| arg("pattern"))
                    .or_else(|| arg("query"))
                    .or_else(|| arg("command"))
                    .unwrap_or_default();
                (ActionKind::Other, relativize(&t), Some(other.to_string()))
            }
        };
        Action {
            seq,
            kind,
            tool: tool.to_string(),
            target,
            detail,
            failed: false,
        }
    }
}

/// One lane's normalised route through the task.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Trajectory {
    pub index: usize,
    pub actions: Vec<Action>,
}

/// One row of the aligned comparison: what each lane did at this point, or
/// nothing if that lane had no equivalent step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AlignedRow {
    /// One entry per lane, in [`Alignment::lanes`] order. `None` means this lane
    /// did not take this step — which is itself a divergence, not missing data.
    pub cells: Vec<Option<Action>>,
    /// Every lane took this step and took it identically.
    pub agree: bool,
    /// Per lane: did it do the same thing as the baseline here?
    ///
    /// With two lanes this is implied by `agree`, but past three columns "nine of
    /// eleven steps diverged" stops being useful on its own — the question
    /// becomes *which* lanes went their own way. Computed here rather than in the
    /// viewer so both agree on what "same" means, which is the whole point of
    /// having a signature.
    pub matches_baseline: Vec<bool>,
}

/// The aligned comparison.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Alignment {
    /// Lane indexes, baseline first — the same order the scoreboard shows.
    pub lanes: Vec<usize>,
    pub rows: Vec<AlignedRow>,
    /// How many rows every lane agreed on. Reported rather than left to be
    /// counted, because "38 of 41 steps identical" is the headline.
    pub agreed: usize,
}

/// Positions in `a` and `b` that pair up in a longest common subsequence.
///
/// Standard dynamic programming over *slots* — see [`Action::slot`]. Matching
/// on slots rather than full signatures is what puts two different edits to one
/// file on the same row instead of in two disjoint ones. Sequences here are one run's
/// tool calls — tens of steps, not thousands — so the quadratic table is not
/// worth avoiding, and being exact matters more than being fast: an approximate
/// alignment silently misattributes divergence, which is worse than no
/// alignment at all.
fn lcs_pairs(a: &[Action], b: &[Action]) -> Vec<(usize, usize)> {
    let (n, m) = (a.len(), b.len());
    let asig: Vec<String> = a.iter().map(|x| x.slot()).collect();
    let bsig: Vec<String> = b.iter().map(|x| x.slot()).collect();
    let mut table = vec![vec![0usize; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            table[i][j] = if asig[i] == bsig[j] {
                table[i + 1][j + 1] + 1
            } else {
                table[i + 1][j].max(table[i][j + 1])
            };
        }
    }
    let mut pairs = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if asig[i] == bsig[j] {
            pairs.push((i, j));
            i += 1;
            j += 1;
        } else if table[i + 1][j] >= table[i][j + 1] {
            i += 1;
        } else {
            j += 1;
        }
    }
    pairs
}

/// Align every lane against the first one and merge into a single row set.
///
/// Rows come out in baseline order, with each lane's unmatched steps sitting in
/// the gap before the baseline step they precede. A lane's own ordering is
/// always preserved — the alignment moves rows, never a lane's sequence.
pub fn align(trajectories: &[Trajectory]) -> Alignment {
    let lanes: Vec<usize> = trajectories.iter().map(|t| t.index).collect();
    if trajectories.is_empty() {
        return Alignment::default();
    }
    let n_lanes = trajectories.len();
    let base = &trajectories[0].actions;

    // For each lane: which of its steps matched which baseline position, and
    // which are insertions bucketed by the baseline position they come before.
    // `base.len()` is the trailing bucket, for steps after the last match.
    let mut matched: Vec<Vec<Option<usize>>> = vec![vec![None; base.len()]; n_lanes];
    let mut gaps: Vec<Vec<Vec<usize>>> = vec![vec![Vec::new(); base.len() + 1]; n_lanes];

    for (li, t) in trajectories.iter().enumerate() {
        if li == 0 {
            for (bi, _) in base.iter().enumerate() {
                matched[0][bi] = Some(bi);
            }
            continue;
        }
        let pairs = lcs_pairs(base, &t.actions);
        let mut consumed = vec![false; t.actions.len()];
        for &(bi, oi) in &pairs {
            matched[li][bi] = Some(oi);
            consumed[oi] = true;
        }
        // Every unconsumed step falls into the bucket before the next baseline
        // position this lane matched, which keeps its relative order intact.
        let mut next_pair = 0usize;
        for (oi, done) in consumed.iter().enumerate() {
            while next_pair < pairs.len() && pairs[next_pair].1 < oi {
                next_pair += 1;
            }
            if *done {
                continue;
            }
            let bucket = pairs.get(next_pair).map(|p| p.0).unwrap_or(base.len());
            gaps[li][bucket].push(oi);
        }
    }

    let mut rows: Vec<AlignedRow> = Vec::new();
    let push_gap = |bucket: usize, rows: &mut Vec<AlignedRow>| {
        let depth = (0..n_lanes)
            .map(|li| gaps[li][bucket].len())
            .max()
            .unwrap_or(0);
        for d in 0..depth {
            let cells: Vec<Option<Action>> = (0..n_lanes)
                .map(|li| {
                    gaps[li][bucket]
                        .get(d)
                        .map(|&oi| trajectories[li].actions[oi].clone())
                })
                .collect();
            // A gap row is by construction a divergence: at least one lane has
            // no step here.
            let matches_baseline = cells
                .iter()
                .map(|c| c.is_none() && cells[0].is_none())
                .collect();
            rows.push(AlignedRow {
                agree: false,
                matches_baseline,
                cells,
            });
        }
    };

    // Indexed by baseline position: `bi` selects a column in every lane's match
    // table, not an element of `base` itself.
    for (bi, _) in base.iter().enumerate() {
        push_gap(bi, &mut rows);
        let cells: Vec<Option<Action>> = (0..n_lanes)
            .map(|li| matched[li][bi].map(|oi| trajectories[li].actions[oi].clone()))
            .collect();
        let agree = cells.iter().all(|c| c.is_some())
            && cells
                .iter()
                .filter_map(|c| c.as_ref().map(|a| a.signature()))
                .collect::<std::collections::HashSet<_>>()
                .len()
                == 1;
        let base_sig = cells[0].as_ref().map(|a| a.signature());
        let matches_baseline = cells
            .iter()
            .map(|c| c.as_ref().map(|a| a.signature()) == base_sig)
            .collect();
        rows.push(AlignedRow {
            cells,
            agree,
            matches_baseline,
        });
    }
    push_gap(base.len(), &mut rows);

    let agreed = rows.iter().filter(|r| r.agree).count();
    Alignment {
        lanes,
        rows,
        agreed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn act(seq: usize, kind: ActionKind, target: &str, detail: Option<&str>) -> Action {
        Action {
            seq,
            kind,
            tool: "t".into(),
            target: target.into(),
            detail: detail.map(|s| s.to_string()),
            failed: false,
        }
    }

    fn traj(index: usize, actions: Vec<Action>) -> Trajectory {
        Trajectory { index, actions }
    }

    #[test]
    fn lane_paths_normalise_to_the_same_file() {
        // The whole comparison rests on this: lanes work in separate worktrees,
        // so without stripping the lane prefix every step would look divergent.
        assert_eq!(
            relativize("/w/.axo-variants/0/lib/orders.ts"),
            "lib/orders.ts"
        );
        assert_eq!(
            relativize("/w/.axo-variants/11/lib/orders.ts"),
            "lib/orders.ts"
        );
        assert_eq!(relativize("./lib/orders.ts"), "lib/orders.ts");
    }

    #[test]
    fn reading_is_fungible_but_editing_is_not() {
        // Two lanes that read the same file are doing the same thing even if the
        // call differs; two lanes that edit it differently are not.
        let r1 = Action::from_call(0, "read_file", Some(&serde_json::json!({"path": "a.ts"})));
        let r2 = Action::from_call(9, "read_file", Some(&serde_json::json!({"path": "a.ts"})));
        assert_eq!(r1.signature(), r2.signature());

        let e1 = Action::from_call(
            0,
            "edit_file",
            Some(&serde_json::json!({"path": "a.ts", "old": "x", "new": "y"})),
        );
        let e2 = Action::from_call(
            0,
            "edit_file",
            Some(&serde_json::json!({"path": "a.ts", "old": "x", "new": "z"})),
        );
        assert_ne!(
            e1.signature(),
            e2.signature(),
            "different edits to one file are the divergence worth showing"
        );
    }

    #[test]
    fn commands_compare_on_substance_not_spacing() {
        let a = Action::from_call(
            0,
            "bash",
            Some(&serde_json::json!({"command": "npm  run   check"})),
        );
        let b = Action::from_call(
            0,
            "bash",
            Some(&serde_json::json!({"command": "npm run check"})),
        );
        assert_eq!(a.signature(), b.signature());
        assert_eq!(a.kind, ActionKind::Run);
        assert_eq!(a.target, "npm");
    }

    #[test]
    fn unknown_tools_survive_normalisation() {
        // An MCP tool must still become a step. Dropping it would make the
        // trajectory a quiet lie about what the lane did.
        let a = Action::from_call(
            0,
            "jira_create_issue",
            Some(&serde_json::json!({"query": "x"})),
        );
        assert_eq!(a.kind, ActionKind::Other);
        assert_eq!(a.tool, "jira_create_issue");
        let b = Action::from_call(0, "slack_post", Some(&serde_json::json!({"query": "x"})));
        assert_ne!(
            a.signature(),
            b.signature(),
            "two different unknown tools are not the same act"
        );
    }

    #[test]
    fn malformed_calls_still_become_steps() {
        let a = Action::from_call(0, "read_file", None);
        assert_eq!(a.kind, ActionKind::Read);
        assert_eq!(a.target, "");
    }

    #[test]
    fn identical_runs_agree_on_every_row() {
        let steps = vec![
            act(0, ActionKind::Read, "a.ts", None),
            act(1, ActionKind::Edit, "a.ts", Some("x → y")),
        ];
        let al = align(&[traj(0, steps.clone()), traj(1, steps)]);
        assert_eq!(al.rows.len(), 2);
        assert_eq!(al.agreed, 2);
        assert!(al.rows.iter().all(|r| r.agree));
    }

    #[test]
    fn a_divergent_edit_is_the_only_row_that_disagrees() {
        // The signal the whole tier exists to produce: same route, one different
        // decision, and the view can collapse everything else.
        let a = vec![
            act(0, ActionKind::Read, "a.ts", None),
            act(1, ActionKind::Edit, "a.ts", Some("x → y")),
            act(2, ActionKind::Run, "npm", Some("npm run check")),
        ];
        let b = vec![
            act(0, ActionKind::Read, "a.ts", None),
            act(1, ActionKind::Edit, "a.ts", Some("x → z")),
            act(2, ActionKind::Run, "npm", Some("npm run check")),
        ];
        let al = align(&[traj(0, a), traj(1, b)]);
        assert_eq!(al.agreed, 2, "the read and the check agree");
        let diverged: Vec<&AlignedRow> = al.rows.iter().filter(|r| !r.agree).collect();
        assert_eq!(diverged.len(), 1);
        let detail: Vec<Option<String>> = diverged[0]
            .cells
            .iter()
            .map(|c| c.as_ref().and_then(|a| a.detail.clone()))
            .collect();
        assert_eq!(
            detail,
            vec![Some("x → y".to_string()), Some("x → z".to_string())],
            "the row must say what each lane did instead"
        );
    }

    #[test]
    fn extra_steps_become_gap_rows_the_other_lane_left_empty() {
        // A lane that flailed through two extra searches should show those as
        // its own rows, not shift the shared steps out of alignment.
        let a = vec![
            act(0, ActionKind::Read, "a.ts", None),
            act(1, ActionKind::Edit, "a.ts", Some("x → y")),
        ];
        let b = vec![
            act(0, ActionKind::Read, "a.ts", None),
            act(1, ActionKind::Search, "foo", None),
            act(2, ActionKind::Search, "bar", None),
            act(3, ActionKind::Edit, "a.ts", Some("x → y")),
        ];
        let al = align(&[traj(0, a), traj(1, b)]);
        assert_eq!(al.agreed, 2, "the read and the edit still line up");
        let gap: Vec<&AlignedRow> = al.rows.iter().filter(|r| !r.agree).collect();
        assert_eq!(gap.len(), 2);
        for r in gap {
            assert!(r.cells[0].is_none(), "baseline did not take this step");
            assert_eq!(r.cells[1].as_ref().unwrap().kind, ActionKind::Search);
        }
        // Order within the diverging lane is preserved.
        assert_eq!(gap_targets(&al), vec!["foo", "bar"]);
    }

    fn gap_targets(al: &Alignment) -> Vec<String> {
        al.rows
            .iter()
            .filter(|r| !r.agree)
            .filter_map(|r| r.cells[1].as_ref().map(|a| a.target.clone()))
            .collect()
    }

    #[test]
    fn three_lanes_align_against_the_baseline() {
        // N from the start, not pairwise: a row is only agreement when *every*
        // lane took the step.
        let base = vec![
            act(0, ActionKind::Read, "a.ts", None),
            act(1, ActionKind::Edit, "a.ts", Some("x → y")),
        ];
        let same = base.clone();
        let differs = vec![
            act(0, ActionKind::Read, "a.ts", None),
            act(1, ActionKind::Edit, "a.ts", Some("x → q")),
        ];
        let al = align(&[traj(0, base), traj(1, same), traj(2, differs)]);
        assert_eq!(al.lanes, vec![0, 1, 2]);
        assert_eq!(
            al.agreed, 1,
            "two of three matching is not agreement — the row is a divergence"
        );
        // ...but the row must still say *which* lane went its own way, or past
        // three columns "diverged" stops being actionable.
        let edit = al.rows.iter().find(|r| !r.agree).unwrap();
        assert_eq!(edit.matches_baseline, vec![true, true, false]);
    }

    #[test]
    fn a_gap_row_marks_only_the_lanes_that_were_also_absent() {
        // Baseline took no step here, so a lane that also took none matches it,
        // and the lane that inserted a step does not.
        let a = vec![act(0, ActionKind::Read, "a.ts", None)];
        let b = vec![
            act(0, ActionKind::Read, "a.ts", None),
            act(1, ActionKind::Search, "foo", None),
        ];
        let c = vec![act(0, ActionKind::Read, "a.ts", None)];
        let al = align(&[traj(0, a), traj(1, b), traj(2, c)]);
        let gap = al.rows.iter().find(|r| !r.agree).unwrap();
        assert_eq!(gap.matches_baseline, vec![true, false, true]);
    }

    #[test]
    fn a_lane_that_did_nothing_still_appears() {
        // The empty-diff lane is a real outcome we already report elsewhere; the
        // trajectory has to show it as a column of nothing, not omit the lane.
        let al = align(&[
            traj(0, vec![act(0, ActionKind::Read, "a.ts", None)]),
            traj(1, vec![]),
        ]);
        assert_eq!(al.lanes, vec![0, 1]);
        assert_eq!(al.rows.len(), 1);
        assert!(!al.rows[0].agree);
        assert!(al.rows[0].cells[1].is_none());
    }

    #[test]
    fn reordered_steps_still_align_where_they_can() {
        // LCS keeps the longest consistent run rather than forcing a lockstep
        // match, so a lane that did the same things in a different order lines
        // up on the subsequence it shares.
        let a = vec![
            act(0, ActionKind::Read, "a.ts", None),
            act(1, ActionKind::Read, "b.ts", None),
            act(2, ActionKind::Edit, "a.ts", Some("x → y")),
        ];
        let b = vec![
            act(0, ActionKind::Read, "b.ts", None),
            act(1, ActionKind::Read, "a.ts", None),
            act(2, ActionKind::Edit, "a.ts", Some("x → y")),
        ];
        let al = align(&[traj(0, a), traj(1, b)]);
        assert!(
            al.agreed >= 2,
            "one shared read plus the shared edit must align, got {}",
            al.agreed
        );
    }

    #[test]
    fn trajectory_round_trips_through_disk() {
        // Written beside the worktrees and read back by a later process, like
        // the rest of a comparison's state.
        let t = traj(
            1,
            vec![Action {
                seq: 0,
                kind: ActionKind::Edit,
                tool: "edit_file".into(),
                target: "lib/orders.ts".into(),
                detail: Some("x → y".into()),
                failed: true,
            }],
        );
        let back: Trajectory = serde_json::from_str(&serde_json::to_string(&t).unwrap()).unwrap();
        assert_eq!(back, t);
    }
}
