//! Host-enforced review of a turn. When a Session team names a reviewer, the
//! admitted graph carries one read-only reviewer node, [`REVIEW_NODE_ID`], that
//! no required Agent depends on, and one review condition,
//! [`REVIEW_CONDITION_ID`], over the turn's required Agents. The host starts the
//! reviewer after the required Agents finish and the required checks pass, and
//! records its verdict as that condition. Only an approval of the exact result
//! it was shown passes it.
use serde::{Deserialize, Serialize};

use crate::execution_content::{
    ActivationEvidenceContent, ExecutionContentError, ExecutionContentStore,
};
use crate::execution_store::DurableTurnSnapshot;
use crate::turn_contract::{
    ConditionId, ConditionKind, ConditionOutcome, GraphNode, LogicalTurnState, TurnGraphSnapshot,
};

/// The review condition. Conditions are scoped to their turn.
pub const REVIEW_CONDITION_ID: &str = "required-review:verdict";
/// The reviewer's node in the turn graph.
pub const REVIEW_NODE_ID: &str = "required-review";
/// Most rounds a person may approve: each is one reviewer activation, and all
/// but the last may send the findings back to the lead.
pub const MAX_REVIEW_ROUNDS: u32 = 3;
/// Rounds when the setting names none.
pub const DEFAULT_REVIEW_ROUNDS: u32 = 2;
/// Most bytes of the change the reviewer is shown.
pub const MAX_CHANGE_BYTES: usize = 48 * 1024;
/// Most bytes of each required Agent's final answer the reviewer is shown.
pub const MAX_ANSWER_BYTES: usize = 16 * 1024;
/// Most bytes of findings recorded and sent back to the lead.
pub const MAX_FINDINGS_BYTES: usize = 16 * 1024;

/// The smallest invocation limit of the Agent that pays for `checks`
/// required checks when a review may run the turn's lead `rounds` times: a
/// pass of the checks per round, never less than the two passes every payer
/// keeps, and its own two captures and answer each time it runs. One round is
/// exactly [`crate::turn_checks::payer_minimum_invocations`].
pub fn payer_minimum_invocations(checks: usize, rounds: u32) -> u32 {
    if checks == 0 {
        return 0;
    }
    crate::turn_checks::check_pass_invocations(checks)
        .saturating_mul(rounds.max(2))
        .saturating_add(3u32.saturating_mul(rounds.max(1)))
}

/// What the admitted graph's review condition names: the reviewer template
/// and how many rounds the host runs before the person decides.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewCriterion {
    pub kind: String,
    pub template_id: String,
    pub max_rounds: u32,
    pub rule: String,
}

impl ReviewCriterion {
    pub fn new(template_id: &str, max_rounds: u32) -> Self {
        Self {
            kind: "required_review".into(),
            template_id: template_id.into(),
            max_rounds,
            rule: "the reviewer approves the exact final answer and repository tree".into(),
        }
    }
}

/// The reviewer node of a graph that carries a required review: an optional
/// node named [`REVIEW_NODE_ID`] beside a review condition named
/// [`REVIEW_CONDITION_ID`]. A graph without both carries no review.
pub fn review_node(graph: &TurnGraphSnapshot) -> Option<&GraphNode> {
    graph.conditions.iter().find(|condition| {
        condition.condition_id.as_str() == REVIEW_CONDITION_ID
            && matches!(condition.kind, ConditionKind::Review { .. })
    })?;
    graph
        .nodes
        .iter()
        .find(|node| node.node_id.as_str() == REVIEW_NODE_ID && !node.required)
}

/// The review condition's identity.
pub fn review_condition_id() -> ConditionId {
    ConditionId::new(REVIEW_CONDITION_ID).expect("review condition identity is valid")
}

/// The criterion a graph's review condition retains, when it carries one.
pub fn review_criterion(
    graph: &TurnGraphSnapshot,
    content: &ExecutionContentStore,
) -> Result<Option<ReviewCriterion>, ExecutionContentError> {
    if review_node(graph).is_none() {
        return Ok(None);
    }
    let Some(ConditionKind::Review { criterion }) = graph
        .conditions
        .iter()
        .find(|condition| condition.condition_id.as_str() == REVIEW_CONDITION_ID)
        .map(|condition| &condition.kind)
    else {
        return Ok(None);
    };
    let ActivationEvidenceContent::Guidance { text } =
        content.resolve_activation_evidence(criterion)?
    else {
        return Err(ExecutionContentError::Invalid(
            "the review criterion is not retained guidance",
        ));
    };
    let criterion: ReviewCriterion = serde_json::from_str(text)
        .map_err(|_| ExecutionContentError::Invalid("the review criterion cannot be read"))?;
    if criterion.kind != "required_review"
        || !(1..=MAX_REVIEW_ROUNDS).contains(&criterion.max_rounds)
    {
        return Err(ExecutionContentError::Invalid(
            "the review criterion is not one this version runs",
        ));
    }
    Ok(Some(criterion))
}

/// A reviewer's verdict, read from the first line of its answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewVerdict {
    Approve,
    Changes,
    /// The answer does not start with a verdict. It never passes.
    Unreadable,
}

/// The verdict and findings of a reviewer's answer. The first line that is
/// not blank must be `VERDICT: APPROVE` or `VERDICT: CHANGES`, ignoring case,
/// surrounding spaces and Markdown emphasis; anything else is
/// [`ReviewVerdict::Unreadable`]. The findings are the rest of the answer,
/// bounded, or the whole answer when the verdict cannot be read.
pub fn parse_verdict(answer: &str) -> (ReviewVerdict, String) {
    let trimmed = answer.trim_start();
    let (first, rest) = trimmed.split_once('\n').unwrap_or((trimmed, ""));
    let line = first
        .trim()
        .trim_matches(|c: char| c == '*' || c == '_' || c == '`' || c == '#')
        .trim();
    let verdict = line
        .split_once(':')
        .filter(|(label, _)| label.trim().eq_ignore_ascii_case("verdict"))
        .map(|(_, value)| {
            let value = value
                .trim()
                .trim_matches(|c: char| c == '*' || c == '_' || c == '`')
                .trim();
            if value.eq_ignore_ascii_case("approve") {
                ReviewVerdict::Approve
            } else if value.eq_ignore_ascii_case("changes") {
                ReviewVerdict::Changes
            } else {
                ReviewVerdict::Unreadable
            }
        })
        .unwrap_or(ReviewVerdict::Unreadable);
    let findings = if verdict == ReviewVerdict::Unreadable {
        answer.trim()
    } else {
        rest.trim()
    };
    (verdict, bounded(findings, MAX_FINDINGS_BYTES))
}

/// At most `max` bytes of `text`, cut on a character boundary, with a note
/// saying how much was left out.
pub fn bounded(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n[Cut at {end} of {} bytes.]", &text[..end], text.len())
}

/// One file's section of a patch, keyed by its `diff --git` header line.
fn patch_sections(patch: &str) -> Vec<(&str, &str)> {
    let mut starts: Vec<usize> = patch
        .match_indices("diff --git ")
        .map(|(index, _)| index)
        .filter(|index| *index == 0 || patch.as_bytes()[index - 1] == b'\n')
        .collect();
    if starts.is_empty() {
        return vec![];
    }
    starts.push(patch.len());
    starts
        .windows(2)
        .map(|pair| {
            let section = &patch[pair[0]..pair[1]];
            let header = section.lines().next().unwrap_or_default();
            (header, section)
        })
        .collect()
}

/// The change a turn made, as the reviewer reads it. Both patches are the
/// repository's changes against HEAD, as the host's captures record them:
/// `before` when the turn began and `after` when its Agents finished. The
/// change is every file section of `after` that is not identical in
/// `before`, and every file `before` changed that `after` no longer does. It
/// is bounded to [`MAX_CHANGE_BYTES`] and says when it was cut.
pub fn describe_change(before: Option<&str>, after: &str, after_complete: bool) -> String {
    let before_sections = before.map(patch_sections).unwrap_or_default();
    let after_sections = patch_sections(after);
    let mut text = String::new();
    for (_, section) in &after_sections {
        if !before_sections
            .iter()
            .any(|(_, earlier)| earlier == section)
        {
            text.push_str(section);
            if !section.ends_with('\n') {
                text.push('\n');
            }
        }
    }
    let reverted: Vec<&str> = before_sections
        .iter()
        .filter(|(header, _)| !after_sections.iter().any(|(later, _)| later == header))
        .map(|(header, _)| *header)
        .collect();
    if !reverted.is_empty() {
        text.push_str(
            "\nThese files had uncommitted changes when the turn began and now match HEAD \
             again:\n",
        );
        for header in reverted {
            text.push_str(header);
            text.push('\n');
        }
    }
    if text.trim().is_empty() {
        text = "The turn changed no files.\n".into();
    }
    let mut described = bounded(&text, MAX_CHANGE_BYTES);
    if described.len() < text.len() {
        described.push_str(" Read the files themselves for the rest of the change.");
    }
    if !after_complete {
        described.push_str(
            "\n[The repository capture recorded only part of the patch; read the files \
             themselves for the rest of the change.]",
        );
    }
    described
}

/// Why the reviewer's view of the tree does not bind its verdict to
/// `candidate`, the tree it was given, or `None` when it does. `before` and
/// `after` are the trees the reviewer's own captures recorded when it
/// started and finished; `None` when it took no capture. A capture that
/// recorded no tree fails.
pub fn binding_failure(
    candidate: Option<&str>,
    before: Option<Option<&str>>,
    after: Option<Option<&str>>,
) -> Option<&'static str> {
    for (capture, when) in [(before, BEFORE_TREE), (after, AFTER_TREE)] {
        match (candidate, capture) {
            (_, None) => {}
            (_, Some(None)) => return Some(UNCAPTURED_TREE),
            (Some(candidate), Some(Some(tree))) if tree != candidate => return Some(when),
            _ => {}
        }
    }
    None
}

const BEFORE_TREE: &str = "The repository changed after the result was captured and before the \
     reviewer started, so its verdict is not about this result. Continue runs the review again.";
const AFTER_TREE: &str = "The repository changed while the reviewer read it, so its verdict is \
     not about one result. Continue runs the review again.";
const UNCAPTURED_TREE: &str = "The reviewer's view of the repository could not be captured, so \
     its verdict cannot be bound to the result. Continue runs the review again.";

/// The review of a turn as the person reads it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TurnReviewView {
    /// The reviewer template.
    pub reviewer: String,
    /// `not_run`, `running`, `approved`, `changes`, `failed`, `skipped` or
    /// `unavailable`.
    pub state: String,
    /// What the state means, in words for the person.
    pub reason: String,
    pub verdict: Option<ReviewVerdict>,
    /// The reviewer's findings, bounded.
    pub findings: String,
    /// The round of the verdict shown: its reviewer generation.
    pub round: Option<u32>,
    pub max_rounds: u32,
    /// Whether the verdict shown is about the current result.
    pub current: bool,
    /// The repository tree the reviewer was shown, when it was captured.
    pub candidate_sha256: Option<String>,
}

/// The recorded proof of one review round, as the host retains it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReviewProof {
    pub kind: String,
    pub round: u32,
    pub max_rounds: u32,
    pub verdict: ReviewVerdict,
    pub findings: String,
    pub passed: bool,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub candidate_sha256: Option<String>,
    /// Whether the host sent the findings back to the lead.
    #[serde(default)]
    pub continued: bool,
}

/// The review of `snapshot`, when its graph carries one: the current
/// verdict, or the latest one and whether a new round is under way.
pub fn project_review(
    snapshot: &DurableTurnSnapshot,
    content: &ExecutionContentStore,
) -> Result<Option<TurnReviewView>, ExecutionContentError> {
    let contract = snapshot.contract();
    let Some(graph) = contract.graph() else {
        return Ok(None);
    };
    let Some(criterion) = review_criterion(graph, content)? else {
        return Ok(None);
    };
    let id = review_condition_id();
    let mut view = TurnReviewView {
        reviewer: criterion.template_id.clone(),
        state: "not_run".into(),
        reason: "The reviewer runs after the required Agents finish and the required checks \
                 pass."
            .into(),
        verdict: None,
        findings: String::new(),
        round: None,
        max_rounds: criterion.max_rounds,
        current: false,
        candidate_sha256: None,
    };
    let current = contract.current_condition(&id);
    let latest = current.or_else(|| {
        contract
            .conditions()
            .iter()
            .rev()
            .find(|observation| observation.condition_id == id)
    });
    if let Some(observation) = latest {
        let ActivationEvidenceContent::Guidance { text } =
            content.resolve_activation_evidence(&observation.evidence)?
        else {
            return Err(ExecutionContentError::Invalid(
                "the review proof is not retained guidance",
            ));
        };
        let proof: ReviewProof = serde_json::from_str(text)
            .map_err(|_| ExecutionContentError::Invalid("the review proof cannot be read"))?;
        view.verdict = Some(proof.verdict);
        view.findings = proof.findings.clone();
        view.round = Some(proof.round);
        view.candidate_sha256 = proof.candidate_sha256.clone();
        view.current = current.is_some();
        let passed = observation.outcome == ConditionOutcome::Passed;
        view.state = if passed {
            "approved"
        } else if proof.verdict == ReviewVerdict::Changes {
            "changes"
        } else {
            "failed"
        }
        .into();
        view.reason = match &proof.reason {
            _ if passed => format!(
                "The reviewer approved this result in round {} of {}.",
                proof.round, proof.max_rounds
            ),
            Some(reason) => reason.clone(),
            None if proof.continued => format!(
                "The reviewer asked for changes in round {} of {}; the host sent its findings \
                 to the lead.",
                proof.round, proof.max_rounds
            ),
            None => format!(
                "The reviewer asked for changes in round {} of {}.",
                proof.round, proof.max_rounds
            ),
        };
        if current.is_none() {
            view.reason = format!(
                "{} The result has changed since; the review runs again on the new result.",
                view.reason
            );
        }
    }
    if current.is_none() {
        let reviewing = review_node(graph).and_then(|node| {
            contract
                .activations()
                .iter()
                .rev()
                .find(|item| item.activation.node_id == node.node_id)
        });
        if contract.state() == Some(LogicalTurnState::Running)
            && reviewing.is_some_and(|item| {
                matches!(
                    item.state,
                    crate::turn_contract::ActivationState::Running
                        | crate::turn_contract::ActivationState::Unstarted
                )
            })
        {
            view.state = "running".into();
            view.reason = "The reviewer is reviewing the result.".into();
        } else if contract.state() == Some(LogicalTurnState::Finished)
            && contract
                .stop_requested()
                .and_then(|intent| intent.partial_finish.as_ref())
                .is_some_and(|selection| selection.missing_condition_ids.contains(&id))
        {
            view.state = "skipped".into();
            view.reason = "Not run; the turn was finished without the review.".into();
        }
    }
    Ok(Some(view))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_review_round_needs_what_required_checks_already_need() {
        assert_eq!(
            payer_minimum_invocations(0, 3),
            0,
            "without checks nobody pays"
        );
        for checks in 1..4 {
            assert_eq!(
                payer_minimum_invocations(checks, 1),
                crate::turn_checks::payer_minimum_invocations(checks)
            );
        }
        // One check: a pass is three invocations.
        assert_eq!(payer_minimum_invocations(1, 2), 12);
        assert_eq!(payer_minimum_invocations(1, 3), 18);
    }

    #[test]
    fn a_verdict_is_the_first_line_and_anything_else_fails_closed() {
        assert_eq!(
            parse_verdict("VERDICT: APPROVE\nNo findings."),
            (ReviewVerdict::Approve, "No findings.".into())
        );
        assert_eq!(
            parse_verdict("\n  **Verdict: changes**\nsrc/lib.rs:3: missing test\n"),
            (ReviewVerdict::Changes, "src/lib.rs:3: missing test".into())
        );
        for answer in [
            "",
            "Looks good to me.\nVERDICT: APPROVE",
            "VERDICT: APPROVED",
            "VERDICT: approve with nits",
            "Decision: APPROVE",
        ] {
            let (verdict, findings) = parse_verdict(answer);
            assert_eq!(verdict, ReviewVerdict::Unreadable, "{answer}");
            assert_eq!(findings, answer.trim());
        }
        let (_, findings) = parse_verdict(&format!("VERDICT: CHANGES\n{}", "é".repeat(9000)));
        assert!(findings.len() < MAX_FINDINGS_BYTES + 64);
        assert!(findings.ends_with("bytes.]"), "{findings}");
    }

    #[test]
    fn the_change_is_what_the_turn_added_to_the_tree_and_is_bounded() {
        let old = "diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-a\n+b\n";
        let gone = "diff --git a/c.txt b/c.txt\n--- a/c.txt\n+++ b/c.txt\n@@ -1 +1 @@\n-c\n+d\n";
        let new = "diff --git a/b.txt b/b.txt\nnew file mode 100644\n--- /dev/null\n+++ b/b.txt\n@@ -0,0 +1 @@\n+x\n";
        let before = format!("{old}{gone}");
        let after = format!("{old}{new}");
        let change = describe_change(Some(&before), &after, true);
        assert!(change.starts_with(new), "{change}");
        assert!(
            !change.contains("-a\n+b"),
            "an earlier change is not the turn's"
        );
        assert!(change.contains("now match HEAD again:\ndiff --git a/c.txt b/c.txt"));
        assert_eq!(
            describe_change(Some(&after), &after, true),
            "The turn changed no files.\n"
        );
        assert_eq!(describe_change(None, new, true), new);
        let large = format!(
            "diff --git a/big b/big\n{}",
            "+line\n".repeat(MAX_CHANGE_BYTES / 4)
        );
        let cut = describe_change(None, &large, false);
        assert!(cut.len() < MAX_CHANGE_BYTES + 512);
        assert!(cut.contains(&format!(
            "[Cut at {MAX_CHANGE_BYTES} of {} bytes.]",
            large.len()
        )));
        assert!(cut.ends_with("the rest of the change.]"));
    }

    /// A verdict binds only when every tree the reviewer saw is the one it
    /// was given: a change after it started or while it read fails it, and so
    /// does a capture that recorded no tree.
    #[test]
    fn a_verdict_is_bound_to_the_tree_the_reviewer_was_given() {
        let tree = Some("t1");
        assert_eq!(binding_failure(tree, None, None), None);
        assert_eq!(binding_failure(tree, Some(tree), Some(tree)), None);
        assert_eq!(
            binding_failure(None, Some(Some("t2")), Some(Some("t2"))),
            None
        );
        assert_eq!(
            binding_failure(tree, Some(Some("t2")), Some(Some("t2"))),
            Some(BEFORE_TREE)
        );
        assert_eq!(
            binding_failure(tree, Some(tree), Some(Some("t2"))),
            Some(AFTER_TREE)
        );
        assert_eq!(
            binding_failure(tree, Some(None), Some(tree)),
            Some(UNCAPTURED_TREE)
        );
    }
}
