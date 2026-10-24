//! Host-enforced review. When a turn's graph carries a required reviewer, the
//! one turn driver starts it after the required Agents finish and the required
//! checks pass, in a fresh conversation whose input is the request, each
//! required Agent's final answer and the change the turn made. Its verdict
//! becomes the review condition. Only an approval of the exact result it was
//! shown passes it; a request for changes sends the findings back to the lead
//! in a new epoch, as a person's Revise would, until the approved rounds run
//! out and the person decides.
use super::*;
use axocoatl_session::execution_content::{ActivationRepositorySnapshot, RepositorySnapshotPhase};
use axocoatl_session::turn_checks::{group_of, CheckGroup};
use axocoatl_session::turn_review::{
    binding_failure, bounded, describe_change, describe_partial_change, earlier_changes,
    fit_description, parse_verdict, review_condition_id, review_criterion, review_node,
    ChangeBudget, DescribedChange, EarlierPaths, ReviewCriterion, ReviewProof, ReviewVerdict,
    MAX_ANSWER_BYTES,
};
use base64::Engine;

/// What the reviewer is shown: the exact result it judges.
struct ReviewCandidate {
    /// The repository tree the Agents left, when it was captured.
    tree: Option<String>,
    prompt: String,
}

/// One review round to start: what the reviewer is shown and where.
struct ReviewRound {
    repository: RepositoryInput,
    /// The retained prompt naming the exact result.
    prompt: EvidenceRef,
    /// The reviewer generation that runs it.
    round: u32,
    /// The host's note asking again for a verdict the previous round's
    /// answer did not carry, and whether that answer is shown beside it;
    /// `None` for a round about a new result.
    reask: Option<(EvidenceRef, bool)>,
}

/// A digest naming one host step of this turn's review.
fn step_digest(value: &impl serde::Serialize) -> Result<String> {
    Ok(format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(value).map_err(error)?)
    ))
}

const UNREADABLE: &str = "The reviewer's answer had no single VERDICT: APPROVE or VERDICT: \
     CHANGES line, so it approves nothing. Continue runs the review again.";
const UNREADABLE_AGAIN: &str = "The reviewer's answer had no single VERDICT: APPROVE or VERDICT: \
     CHANGES line, even after the host asked it again, so it approves nothing. Continue runs \
     the review again.";
/// What the host tells a reviewer whose answer carried no readable verdict
/// when it asks again, in the next round, about the same result.
const REASK: &str = "Your previous answer did not contain a verdict line. Reply with exactly \
     `VERDICT: APPROVE` or `VERDICT: CHANGES` on its own line, followed by findings.";
/// What the host tells such a reviewer instead when that answer takes more
/// than the room its request keeps for it ([`reask_room`]), so it is not
/// shown again.
const REASK_UNSHOWN: &str = "Your previous answer did not contain a verdict line, and it is too \
     long to show again here. Review the result again and reply with exactly `VERDICT: APPROVE` \
     or `VERDICT: CHANGES` on its own line, followed by findings.";
const NOT_CAPTURED: &str = "The repository could not be captured after the Agents finished, so \
     the reviewer would not see the exact result. Continue runs the review again.";
/// The rule the host appends to every reviewer prompt, so the lead can answer
/// each finding by its id and the host can record each answer.
pub(crate) const FINDING_ID_RULE: &str = "Number each finding F1, F2, ... at the start of its \
     line, such as `F1: src/lib.rs:3: the loop skips the last item; stop at len`. The lead \
     answers each finding by its id.";
/// The host's instruction to the lead that receives review findings: answer
/// each one, so the run records the writer's decision and reason.
pub(crate) const ADJUDICATION_RULE: &str = "Answer every finding below in an ADJUDICATIONS \
     block: a fenced JSON array of {\"id\":\"F1\",\"decision\":\"accept\"|\"reject\",\
     \"reason\":\"...\"}, one entry per finding, then fix each finding you accept.";
/// Said when the reviewer did not number its findings: they are answered as
/// one finding.
const UNNUMBERED: &str = "The reviewer did not number its findings, so they are one finding, F1.";

/// What the reviewer's prompt says when the change it shows is not all of
/// it.
pub(crate) const PARTIAL_CHANGE_RULE: &str = "Not all of the change is shown below: the \
     notes in the Change section say what was left out (the data of binary files, which are \
     named with their sizes, or text cut to fit your context). You have not seen that part. \
     Read the files those notes name before you judge them, and do not approve code you have \
     not read.";

/// The opening of the reviewer's prompt about the result whose repository
/// tree is `tree`: what to judge, the verdict line, how to write findings,
/// and, when the change shown is `partial`, that it is not all of it.
fn reviewer_prompt_head(tree: Option<&str>, partial: bool) -> String {
    format!(
        "You are the required reviewer of this turn. Its request is above. Review the \
         result below against that request: the final answer of each Agent that did the \
         work and the change the turn made to the repository. Check the change against \
         every contract and edge case the project's instructions, docs and tests \
         describe, one by one. You cannot change files; you may read them.\n\nAnswer in \
         this form. The first line is exactly one of:\n\
         VERDICT: APPROVE\nVERDICT: CHANGES\nThen list each finding on its own line as \
         path:line: what is wrong and what to change. {FINDING_ID_RULE} Approve only when \
         nothing must change.\n\n{}Repository tree reviewed: {}\n",
        if partial {
            format!("{PARTIAL_CHANGE_RULE}\n\n")
        } else {
            String::new()
        },
        tree.unwrap_or("not captured")
    )
}

/// Tokens the host counts for each image a request carries, whatever its
/// size (`axocoatl_token::ApproximateCounter`).
const IMAGE_TOKENS: usize = 1_024;

/// Tokens of a reviewer's request the host does not count where it builds
/// the prompt: the activation input's JSON around the guidance (about 700
/// tokens with the repository's description), the definitions of the
/// reviewer's read-only tools (about 550 for the four a loadout's reviewer
/// has), the round's note and the messages' framing, with room for a few
/// more tools.
const REVIEW_REQUEST_RESERVE: usize = 2_048;

/// The context and output allowance a reviewer is fitted to when its model
/// has no native provider profile the host can read: the smallest context
/// onboarding configures for a run's local model
/// ([`axocoatl_config::loadout::MIN_RUN_CONTEXT_TOKENS`]) and the output
/// bound of an Agent that names none.
const FALLBACK_REVIEWER_WINDOW: (usize, usize) = (
    axocoatl_config::loadout::MIN_RUN_CONTEXT_TOKENS as usize,
    axocoatl_config::loadout::DEFAULT_MAX_OUTPUT_TOKENS,
);

/// The tokens `text` takes in a reviewer's request as the host counts a
/// request against its model's context (cl100k): the more of the text and
/// the text as the reviewer's JSON input carries it. Without the counter, its
/// bytes, which no count passes.
pub(crate) fn review_tokens(text: &str) -> usize {
    static COUNTER: std::sync::OnceLock<Option<axocoatl_token::ApproximateCounter>> =
        std::sync::OnceLock::new();
    let json = serde_json::to_string(text).unwrap_or_default();
    match COUNTER.get_or_init(|| axocoatl_token::ApproximateCounter::new().ok()) {
        Some(counter) => {
            use axocoatl_token::TokenCounter;
            counter.count_text(text).max(counter.count_text(&json))
        }
        None => json.len(),
    }
}

/// Most tokens of a previous answer a request that asks the reviewer again
/// for its verdict shows beside the result ([`reask_room`]).
const MAX_REASK_ANSWER_TOKENS: usize = 2_048;

/// Tokens of the record that carries a previous answer beside its text: the
/// activation it came from and its usage, as the reviewer's JSON input
/// carries them.
const REASK_ANSWER_FRAME: usize = 256;

/// The tokens of its previous answer a reviewer whose model holds `context`
/// tokens and reserves `output` of them for each answer is shown again when
/// the host asks it again for a verdict: a sixteenth of what one request may
/// hold beside the output allowance, at most [`MAX_REASK_ANSWER_TOKENS`].
/// Every round's request keeps that room, so the result is shown the same
/// way when it is asked again; a longer answer is not shown again.
pub(crate) fn reask_room(context: usize, output: usize) -> usize {
    let limit = (context as f32 * axocoatl_token::COMPRESSION_TRIGGER_PCT) as usize;
    (limit.saturating_sub(output) / 16).min(MAX_REASK_ANSWER_TOKENS)
}

/// The tokens the change may take in the request of a reviewer whose model
/// holds `context` tokens and reserves `output` of them for each answer,
/// when the rest of its prompt takes `spent` as the host counts it. The host
/// refuses a request whose prompt and output allowance pass
/// [`axocoatl_token::COMPRESSION_TRIGGER_PCT`] of the context (27,852 of
/// 32,768); the change takes at most three quarters of what that leaves
/// beside the output allowance, less what the rest of the request takes, so
/// a provider that counts more than the host does still holds it.
pub(crate) fn change_budget(context: usize, output: usize, spent: usize) -> usize {
    let limit = (context as f32 * axocoatl_token::COMPRESSION_TRIGGER_PCT) as usize;
    (limit.saturating_sub(output) / 4 * 3)
        .saturating_sub(spent.saturating_add(REVIEW_REQUEST_RESERVE))
}

/// The wait before a review round a transient provider failure ended runs
/// again when the provider stated no `Retry-After`: 4 seconds, twice the
/// provider retry's own backoff, doubled for each of the reviewer's rounds
/// this turn already ran again (`earlier`), and at most the longest
/// `Retry-After` the host honors.
pub(crate) fn review_rerun_backoff(earlier: usize) -> std::time::Duration {
    let doubled = u32::try_from(earlier)
        .ok()
        .and_then(|earlier| 2u32.checked_pow(earlier))
        .and_then(|factor| (crate::provider_retry::DEFAULT_RETRY_BACKOFF * 2).checked_mul(factor))
        .unwrap_or(crate::provider_retry::MAX_RETRY_AFTER);
    doubled.min(crate::provider_retry::MAX_RETRY_AFTER)
}

/// How an activation records a native Ollama response that held neither an
/// answer nor a tool call, as `axocoatl-llm-ollama`'s native stream words
/// it: gpt-oss:20b reviewing a fix called a `search` tool it was never
/// offered, which Ollama dropped, and wrote nothing else.
const NATIVE_NO_ANSWER: &str = "LLM provider error: Streaming error: native Ollama: native \
     response has no final content or tool call";

/// Why a reviewer that did not finish can in a fresh conversation, read
/// from the host-written first line of its recorded failure: its model kept
/// calling tools and gave no answer, even when asked for its final answer
/// without tools; gave no answer and called no tool it was offered; used
/// every tool round of its activation; or reached its token limit for the
/// activation. A new activation of the same round starts without that
/// conversation and with its own token limit. `None` for any other failure:
/// one the same request meets again (a request larger than the model's
/// context, a Session budget used up, a refused admission), a refusal, or a
/// provider's, which [`crate::provider_retry`] judges.
pub(crate) fn rerun_cause_of_text(text: &str) -> Option<&'static str> {
    let first = text.lines().next()?;
    let line = first.strip_prefix("Activation failed: ")?;
    if line.starts_with("Tool call failed: agent tool loop - ") {
        return Some("its model kept calling tools and gave no answer");
    }
    if line == NATIVE_NO_ANSWER {
        return Some("its model gave no answer and called no tool it was offered");
    }
    if line.starts_with("This Agent reached its token limit for this activation") {
        return Some("it reached its token limit for the activation");
    }
    let view = axocoatl_session::execution_content::classify_activation_failure(first)?;
    (view.class == "round_limit").then_some("it used every tool round of its activation")
}

/// Most bytes of a reviewer's own failure a review's reason quotes.
const MAX_FAILURE_CAUSE_BYTES: usize = 300;

/// The cause a reviewer's recorded failure states on its first line, in
/// words for the person: a request larger than its model takes says how
/// many tokens it needed and the limit; any other cause is quoted, at most
/// [`MAX_FAILURE_CAUSE_BYTES`].
fn failure_cause(text: &str) -> Option<String> {
    let line = text
        .lines()
        .next()?
        .strip_prefix("Activation failed: ")?
        .trim()
        .trim_end_matches('.');
    if let Some((required, limit)) = line
        .strip_prefix("Current request needs ")
        .and_then(|rest| rest.split_once(" context tokens, exceeding limit "))
        .and_then(|(required, limit)| {
            Some((required.parse::<u64>().ok()?, limit.parse::<u64>().ok()?))
        })
    {
        return Some(format!(
            "its request needed {} tokens of its model's context, more than the {} one request \
             may take with its output allowance, so the change, the Agents' answers and its \
             instructions did not fit",
            super::repository_snapshot::group_digits(required),
            super::repository_snapshot::group_digits(limit)
        ));
    }
    if line.is_empty() {
        return None;
    }
    if line.len() <= MAX_FAILURE_CAUSE_BYTES {
        return Some(line.to_owned());
    }
    let mut end = MAX_FAILURE_CAUSE_BYTES;
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    Some(format!("{}...", &line[..end]))
}

/// The note that sends review round `round`'s `findings` back to the lead.
fn revision_note(reviewer: &str, round: u32, max_rounds: u32, findings: &str) -> String {
    // Without any id the whole text is the one finding F1.
    let unnumbered = match axocoatl_session::review_adjudication::split_findings(findings) {
        Ok(split) if split.len() == 1 && split[0].text == findings.trim() => {
            format!("{UNNUMBERED}\n\n")
        }
        _ => String::new(),
    };
    format!(
        "The required reviewer ({reviewer}) asked for changes in review round {round} of \
         {max_rounds}. {ADJUDICATION_RULE}\n\n{unnumbered}{findings}\n\nAddress each finding, \
         then give your final answer again. The host runs the required checks and the review \
         again on your new result."
    )
}

impl DispatchState {
    /// Whether `node` is this turn's required reviewer.
    pub(super) fn is_review_node(&self, node: &TurnNodeId) -> Result<bool> {
        let snapshot = self.canonical.snapshot(&self.turn_id).map_err(error)?;
        Ok(snapshot
            .contract()
            .graph()
            .and_then(review_node)
            .is_some_and(|reviewer| reviewer.node_id == *node))
    }

    /// Whether `node` runs its template in a conversation of its own: a
    /// delegated helper or the required reviewer.
    pub(super) fn owns_instance_conversation(&self, node: &TurnNodeId) -> Result<bool> {
        Ok(self.native_child_origin(node)?.is_some() || self.is_review_node(node)?)
    }

    /// The final answer an accepted activation recorded.
    fn accepted_answer(
        &self,
        snapshot: &DurableTurnSnapshot,
        activation: &ActivationRef,
    ) -> Result<String> {
        let reservation = self
            .content
            .activation_output_reservation(snapshot, activation)
            .map_err(error)?
            .ok_or_else(|| error("an accepted answer has no output reservation"))?;
        let output = self
            .content
            .activation_output_settlement(&reservation)
            .map_err(error)?
            .ok_or_else(|| error("an accepted answer is missing"))?;
        Ok(output.content().output.text.clone())
    }

    /// The capture `phase` of `item`: `None` when its profile cannot capture
    /// the repository, `Some(None)` when it could but no capture recorded a
    /// tree.
    fn review_capture(
        &self,
        snapshot: &DurableTurnSnapshot,
        item: &ContractActivation,
        phase: RepositorySnapshotPhase,
    ) -> Result<Option<Option<ActivationRepositorySnapshot>>> {
        let ActivationEvidenceContent::Definition { profile, .. } = &self
            .content
            .resolve_activation_evidence(&item.input.definition.snapshot)
            .map_err(error)?
        else {
            return Err(error("a reviewed Agent's definition is missing"));
        };
        if !matches!(item.input.repository, RepositoryInput::Recorded { .. })
            || super::repository_snapshot::capture_tool(profile).is_none()
        {
            return Ok(None);
        }
        Ok(Some(
            self.content
                .repository_snapshots(snapshot, &item.activation)
                .map_err(error)?
                .into_iter()
                .rev()
                .map(|capture| capture.content)
                .find(|capture| capture.phase == phase)
                .filter(|capture| capture.unavailable.is_none() && capture.tree_sha256.is_some()),
        ))
    }

    /// The context of the reviewer's model and the output allowance each of
    /// its requests reserves, from the native provider profile its
    /// definition retained when its team was applied (so every round of a
    /// turn is shown the change the same way), or
    /// [`FALLBACK_REVIEWER_WINDOW`] without one.
    fn reviewer_window(&self, graph: &TurnGraphSnapshot) -> Result<(usize, usize)> {
        let Some(reviewer) = review_node(graph) else {
            return Ok(FALLBACK_REVIEWER_WINDOW);
        };
        Ok(self
            .content
            .resolve_provider_profile(&self.canonical, &reviewer.definition.snapshot)
            .map_err(error)?
            .and_then(|(_, profile)| {
                super::native::retained_request_window(profile.configuration())
            })
            .unwrap_or(FALLBACK_REVIEWER_WINDOW))
    }

    /// The tokens of the reviewer's request the prompt does not hold but the
    /// host counts where it builds it: the turn's request with the context
    /// it captured, the reviewer's own instructions, and the attachments its
    /// admission gave it.
    fn review_request_tokens(
        &self,
        snapshot: &DurableTurnSnapshot,
        graph: &TurnGraphSnapshot,
    ) -> Result<usize> {
        let mut tokens = 0;
        if let Some((_, request)) = self
            .content
            .retained_request(snapshot.turn_id())
            .map_err(error)?
        {
            tokens += review_tokens(&request.effective_input);
            if !request.context.is_empty() {
                tokens += review_tokens(&serde_json::to_string(&request.context).map_err(error)?);
            }
        }
        if let Some(reviewer) = review_node(graph) {
            if let ActivationEvidenceContent::Definition { configuration, .. } = self
                .content
                .resolve_activation_evidence(&reviewer.definition.snapshot)
                .map_err(error)?
            {
                tokens += match serde_json::from_str::<axocoatl_core::AgentConfig>(&configuration) {
                    Ok(config) => config.system_prompt.as_deref().map_or(0, review_tokens),
                    Err(_) => review_tokens(&configuration),
                };
            }
            if let Some((_, admission)) = self
                .content
                .turn_admission(&self.canonical, &self.turn_id)
                .map_err(error)?
            {
                if let Some(admitted) = admission
                    .nodes
                    .iter()
                    .find(|node| node.node_id == reviewer.node_id)
                {
                    for attachment in &admitted.attachments {
                        tokens += self.attachment_tokens(attachment)?;
                    }
                }
            }
        }
        Ok(tokens)
    }

    /// The tokens one of the reviewer's attachments takes in its request:
    /// a text attachment as its input carries it, and a binary one as its
    /// description there, the visual allowance the host counts for an image
    /// ([`IMAGE_TOKENS`]) and the text it adds for one: the text extracted
    /// from it, or a document's own text.
    fn attachment_tokens(&self, reference: &EvidenceRef) -> Result<usize> {
        Ok(
            match self
                .content
                .resolve_activation_evidence(reference)
                .map_err(error)?
            {
                evidence @ ActivationEvidenceContent::Attachment { .. } => {
                    review_tokens(&serde_json::to_string(&evidence).map_err(error)?)
                }
                ActivationEvidenceContent::BinaryAttachment { attachment } => {
                    let image = attachment
                        .media_type()
                        .split('/')
                        .next()
                        .is_some_and(|kind| kind.trim().eq_ignore_ascii_case("image"));
                    let described = serde_json::json!({
                        "kind": "binary_attachment",
                        "reference_id": attachment.reference_id(),
                        "name": attachment.name(),
                        "media_type": attachment.media_type(),
                        "source_sha256": attachment.source_sha256(),
                        "byte_len": attachment.byte_len(),
                        "representation": "attachment_text",
                    })
                    .to_string();
                    let text = match attachment.extracted_text() {
                        Some(text) => review_tokens(text),
                        None if image => 0,
                        None => {
                            let source = attachment.to_attachment().map_err(error)?;
                            review_tokens(&String::from_utf8_lossy(&source.bytes))
                        }
                    };
                    review_tokens(&described)
                        + review_tokens(&format!("<attachment name=\"{}\">", attachment.name()))
                        + if image { IMAGE_TOKENS } else { 0 }
                        + text
                }
                _ => 0,
            },
        )
    }

    /// The result `activations` left, as the reviewer is shown it, or why
    /// it cannot be shown exactly.
    fn review_candidate(
        &self,
        snapshot: &DurableTurnSnapshot,
        activations: &[&ContractActivation],
    ) -> Result<std::result::Result<ReviewCandidate, &'static str>> {
        let contract = snapshot.contract();
        let graph = contract
            .graph()
            .ok_or_else(|| error("a reviewed turn has no graph"))?;
        // The tree the checks passed on, or else the one the last Agent to
        // finish left.
        let mut after = None;
        let mut capturable = false;
        if let Some((group, _)) =
            group_of(graph).filter(|(group, _)| *group == CheckGroup::required())
        {
            let id = ConditionId::new(group.ready_id()).map_err(error)?;
            let observation = contract
                .current_condition(&id)
                .ok_or_else(|| error("the required checks have no current readiness"))?;
            let ActivationEvidenceContent::Guidance { text } = &self
                .content
                .resolve_activation_evidence(&observation.evidence)
                .map_err(error)?
            else {
                return Err(error("the readiness proof is not retained guidance"));
            };
            let proof: serde_json::Value = serde_json::from_str(text).map_err(error)?;
            let reference = proof
                .get("after")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| error("the readiness proof names no capture"))?;
            capturable = true;
            for item in activations {
                after = after.or(self
                    .content
                    .repository_snapshots(snapshot, &item.activation)
                    .map_err(error)?
                    .into_iter()
                    .find(|capture| capture.reference.as_str() == reference)
                    .map(|capture| capture.content)
                    .filter(|capture| capture.tree_sha256.is_some()));
            }
        } else {
            let records = self.canonical.turn_records(&self.turn_id).map_err(error)?;
            let last = records.iter().rev().find_map(|record| match &record.event {
                TurnContractEvent::AcceptActivation { activation, .. }
                    if record.turn_id == self.turn_id =>
                {
                    activations
                        .iter()
                        .find(|item| item.activation == *activation)
                        .copied()
                }
                _ => None,
            });
            if let Some(last) = last {
                if let Some(capture) =
                    self.review_capture(snapshot, last, RepositorySnapshotPhase::After)?
                {
                    capturable = true;
                    after = capture;
                }
            }
        }
        if capturable && after.is_none() {
            return Ok(Err(NOT_CAPTURED));
        }
        // What the tree held when the turn's first Agent started.
        let mut before = None;
        let scope: Vec<_> = activations
            .iter()
            .map(|item| &item.activation.node_id)
            .collect();
        if let Some(first) = contract
            .activations()
            .iter()
            .find(|item| scope.contains(&&item.activation.node_id))
        {
            before = self
                .review_capture(snapshot, first, RepositorySnapshotPhase::Before)?
                .flatten();
        }
        let patch = |capture: &ActivationRepositorySnapshot| {
            capture
                .patch_base64
                .as_deref()
                .and_then(|encoded| {
                    base64::engine::general_purpose::STANDARD
                        .decode(encoded)
                        .ok()
                })
                .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        };
        let tree = after.as_ref().and_then(|after| after.tree_sha256.clone());
        let mut answers = String::new();
        for item in activations {
            let name = super::turn_checks::agent_name(
                &self.content,
                Some(graph),
                &item.activation.node_id,
            );
            let answer = self.accepted_answer(snapshot, &item.activation)?;
            answers.push_str(&format!(
                "\n## Final answer of {name}\n{}\n",
                bounded(&answer, MAX_ANSWER_BYTES)
            ));
        }
        // The change fits the reviewer's context beside the rest of its
        // request: the head (counted with the note a partial change adds),
        // the answers, the turn's request, the reviewer's instructions and
        // attachments, and the room a request asking it again for a verdict
        // keeps for its previous answer.
        let (context, output) = self.reviewer_window(graph)?;
        let spent = review_tokens(&reviewer_prompt_head(tree.as_deref(), true))
            + review_tokens(&answers)
            + review_tokens("\n## Change\n")
            + self.review_request_tokens(snapshot, graph)?
            + reask_room(context, output)
            + REASK_ANSWER_FRAME
            + review_tokens(REASK_UNSHOWN);
        let budget = ChangeBudget {
            tokens: change_budget(context, output, spent),
            count: &review_tokens,
        };
        let change = match &after {
            None => DescribedChange {
                text: "No repository capture was taken: the reviewed Agents cannot run \
                       commands or change files, so there is no change to show."
                    .to_owned(),
                partial: false,
            },
            Some(after) => {
                let moved = before
                    .as_ref()
                    .is_some_and(|before| before.head != after.head);
                let earlier = before.as_ref().filter(|_| !moved);
                // A patch too large for one capture is never shown as if it
                // were whole: the reviewer is told so, and given the files
                // the turn changed and the first part of the patch.
                let mut change = if after.patch_complete {
                    let Some(after_patch) = patch(after) else {
                        return Ok(Err(NOT_CAPTURED));
                    };
                    match earlier.filter(|before| !before.patch_complete) {
                        None => describe_change(
                            earlier.and_then(patch).as_deref(),
                            &after_patch,
                            after.patch_complete,
                            budget,
                        ),
                        // The patch the turn began with is not recorded:
                        // the change is every uncommitted change, with the
                        // files that already differed from HEAD named, and
                        // those that no longer do, from the captures' lines
                        // of the paths that differ from HEAD.
                        Some(before) => {
                            let head = "The capture taken when the turn began could not record \
                                        its whole patch, so this is every uncommitted change \
                                        against HEAD, including any made before the turn.\n";
                            let earlier = match (&before.changed_manifest, &after.changed_manifest)
                            {
                                (Some(before), Some(after)) => fit_description(
                                    earlier_changes(before, after),
                                    ChangeBudget {
                                        tokens: budget.tokens / 4,
                                        count: &review_tokens,
                                    },
                                ),
                                _ => String::new(),
                            };
                            let mut change = describe_change(
                                None,
                                &after_patch,
                                after.patch_complete,
                                ChangeBudget {
                                    tokens: budget.tokens.saturating_sub(
                                        review_tokens(head) + review_tokens(&earlier),
                                    ),
                                    count: &review_tokens,
                                },
                            );
                            change.text = format!("{head}{}{earlier}", change.text);
                            change
                        }
                    }
                } else {
                    let named = before
                        .as_ref()
                        .filter(|before| !moved && before.changed_manifest.is_none())
                        .and_then(|before| {
                            let encoded = before.patch_base64.as_deref()?;
                            let bytes = base64::engine::general_purpose::STANDARD
                                .decode(encoded)
                                .ok()?;
                            crate::run_attribution::patch_paths(&bytes)
                        });
                    let listed = match (&before, moved) {
                        (Some(before), true) if before.changed_manifest.is_some() => {
                            EarlierPaths::OtherHead
                        }
                        (Some(before), false) => match (&before.changed_manifest, &named) {
                            (Some(lines), _) => EarlierPaths::Listed(lines),
                            (None, Some(named)) => EarlierPaths::Patched(named),
                            (None, None) => EarlierPaths::NotListed,
                        },
                        _ => EarlierPaths::NotListed,
                    };
                    DescribedChange {
                        text: fit_description(
                            describe_partial_change(
                                listed,
                                after.changed_manifest.as_deref(),
                                &after.patch_prefix,
                                after.patch_bytes,
                            ),
                            budget,
                        ),
                        partial: true,
                    }
                };
                if moved {
                    change.text = format!(
                        "HEAD moved during the turn, so this is every uncommitted change against \
                         the new HEAD, including any made before the turn.\n{}",
                        change.text
                    );
                }
                change
            }
        };
        let mut prompt = reviewer_prompt_head(tree.as_deref(), change.partial);
        prompt.push_str(&answers);
        prompt.push_str(&format!("\n## Change\n{}", change.text));
        Ok(Ok(ReviewCandidate { tree, prompt }))
    }

    /// Record the review condition over `activations` from `proof`. The
    /// caller records at most one verdict per epoch: none once the condition
    /// has a current observation.
    fn record_review(
        &mut self,
        epoch: &ExecutionEpochId,
        activations: Vec<ActivationRef>,
        proof: serde_json::Value,
        passed: bool,
    ) -> Result<()> {
        let text = proof.to_string();
        let evidence = self
            .content
            .retain_activation_evidence(ActivationEvidenceContent::Guidance { text })
            .map_err(error);
        let evidence = self.fail_closed(evidence)?.reference().clone();
        let command = format!(
            "host-review-verdict-{}",
            &step_digest(&(epoch, &evidence))?[..48]
        );
        let appended = self.append(
            &command,
            TurnContractEvent::RecordCondition {
                epoch_id: epoch.clone(),
                condition_id: review_condition_id(),
                activations,
                outcome: if passed {
                    ConditionOutcome::Passed
                } else {
                    ConditionOutcome::Failed
                },
                evidence,
            },
        );
        self.fail_closed(appended)
    }

    /// The continuation that sends `findings` of review round `round` to the
    /// turn's lead: a Revise of its accepted result in a new epoch that runs
    /// every condition again, as a person's Revise of a paused turn does. The
    /// reason there is none: more than one Agent's result was reviewed, or
    /// the lead cannot take another instruction or run again.
    fn review_revision(
        &mut self,
        snapshot: &DurableTurnSnapshot,
        criterion: &ReviewCriterion,
        round: u32,
        findings: &str,
        activations: &[ActivationRef],
    ) -> Result<std::result::Result<ContinuationPlan, String>> {
        let contract = snapshot.contract();
        let graph = contract
            .graph()
            .ok_or_else(|| error("a reviewed turn has no graph"))?;
        let required = |node: &TurnNodeId| {
            graph
                .nodes
                .iter()
                .any(|item| item.node_id == *node && item.required)
        };
        let leads: Vec<_> = graph
            .nodes
            .iter()
            .filter(|node| {
                node.required
                    && !graph
                        .dependencies
                        .iter()
                        .any(|edge| edge.parent == node.node_id && required(&edge.child))
            })
            .collect();
        let [lead] = leads.as_slice() else {
            return Ok(Err(
                "More than one Agent's result was reviewed, so the host does not choose which \
                 one revises. Revise one of them with the findings, change the files yourself \
                 and Continue the review, or Finish."
                    .into(),
            ));
        };
        let previous = contract
            .activations()
            .iter()
            .rev()
            .find(|item| item.activation.node_id == lead.node_id)
            .filter(|item| item.state == ActivationState::Accepted && item.output.is_some())
            .ok_or_else(|| error("the reviewed lead has no accepted result"))?;
        if previous
            .input
            .guidance
            .len()
            .saturating_add(previous.input.attachments.len())
            .saturating_add(previous.input.parents.len())
            >= MAX_INPUT_REFERENCES
        {
            return Ok(Err(
                "The lead's input has no room for another instruction, so the host cannot send \
                 it the findings. Finish, or start a new turn with them."
                    .into(),
            ));
        }
        let instruction = revision_note(
            &criterion.template_id,
            round,
            criterion.max_rounds,
            findings,
        );
        let instruction = self
            .content
            .retain_activation_evidence(ActivationEvidenceContent::Guidance { text: instruction })
            .map_err(error);
        let instruction = self.fail_closed(instruction)?.reference().clone();
        let source = contract
            .epochs()
            .last()
            .ok_or_else(|| error("a reviewed turn has no epoch"))?
            .id
            .clone();
        let digest = step_digest(&(
            "host-review-revision-v1",
            snapshot.journal_id(),
            &source,
            round,
        ))?;
        let epoch =
            ExecutionEpochId::new(format!("epoch-review-{}", &digest[..48])).map_err(error)?;
        let mut input = previous.input.clone();
        input.manifest_id =
            InputManifestId::new(format!("input-review-{}", &digest[..48])).map_err(error)?;
        input.activation.activation_id =
            ActivationId::new(format!("activation-review-{}", &digest[..48])).map_err(error)?;
        input.activation.execution_epoch_id = epoch.clone();
        input.activation.generation = previous
            .activation
            .generation
            .checked_add(1)
            .ok_or_else(|| error("activation generation overflow"))?;
        input.guidance.push(instruction.clone());
        input.revision_context = Some(RevisionContext {
            activation: previous.activation.clone(),
            output: previous
                .output
                .clone()
                .ok_or_else(|| error("the reviewed lead has no output"))?,
        });
        if let Err(refused) = self.validate_control_input(snapshot, &input) {
            return Ok(Err(format!(
                "The lead cannot run again, so the host cannot send it the findings: {}. \
                 Revise it yourself after fixing that, or Finish.",
                refused
                    .to_string()
                    .strip_prefix("Session dispatch: ")
                    .unwrap_or(&refused.to_string())
            )));
        }
        let invalidate = self.human_revision_invalidation(&previous.activation)?;
        let selections = super::host_controls::revision_selections(
            contract,
            &previous.activation,
            &input,
            &invalidate,
            &instruction,
            &instruction,
        )?;
        let plan = ContinuationPlan {
            source_epoch_id: source.clone(),
            epoch_id: epoch,
            selections,
            condition_runs: graph
                .conditions
                .iter()
                .map(|condition| condition.condition_id.clone())
                .collect(),
        };
        // The verdict, the pause and the continuation must all apply; a
        // continuation the contract would refuse leaves the turn needing
        // attention with the reason instead.
        let mut preview = contract.clone();
        for (operation, event) in [
            (
                "verdict",
                TurnContractEvent::RecordCondition {
                    epoch_id: source.clone(),
                    condition_id: review_condition_id(),
                    activations: activations.to_vec(),
                    outcome: ConditionOutcome::Failed,
                    evidence: instruction.clone(),
                },
            ),
            ("pause", TurnContractEvent::PauseEpoch { epoch_id: source }),
            (
                "continue",
                TurnContractEvent::Continue { plan: plan.clone() },
            ),
        ] {
            let envelope = TurnContractEnvelope {
                schema_version: TURN_CONTRACT_SCHEMA_VERSION,
                command_id: CommandId::new(format!("preview-review-{operation}")).map_err(error)?,
                expected_revision: preview.revision(),
                session_id: snapshot.owner().session_id.clone(),
                turn_id: snapshot.turn_id().clone(),
                event,
            };
            if let Err(refused) = preview.apply(&envelope) {
                return Ok(Err(format!(
                    "The host could not send the findings to the lead: {refused}. Revise it \
                     yourself with them, or Finish."
                )));
            }
        }
        Ok(Ok(plan))
    }

    /// Continue in a new epoch that prepares the reviewer's first activation,
    /// `input`: a continuation left the never-started reviewer blocked in the
    /// current one, and only a new epoch can start it. Everything else keeps
    /// its accepted result or stays blocked by `blocker`; only the review runs.
    /// The reason when the contract would refuse it.
    fn unblock_review(
        &mut self,
        snapshot: &DurableTurnSnapshot,
        input: ActivationInputManifest,
        blocker: &EvidenceRef,
    ) -> Result<std::result::Result<(), String>> {
        let contract = snapshot.contract();
        let graph = contract
            .graph()
            .ok_or_else(|| error("a reviewed turn has no graph"))?;
        let source = contract
            .epochs()
            .last()
            .ok_or_else(|| error("a reviewed turn has no epoch"))?
            .id
            .clone();
        let accepted = contract.current_accepted_activations();
        let mut selections = Vec::with_capacity(graph.nodes.len());
        for node in &graph.nodes {
            if node.node_id == input.activation.node_id {
                selections.push(ContinuationSelection::PrepareUnmaterialized {
                    input: Box::new(input.clone()),
                });
                continue;
            }
            selections.push(
                match contract
                    .activations()
                    .iter()
                    .rev()
                    .find(|item| item.activation.node_id == node.node_id)
                {
                    Some(item)
                        if accepted
                            .iter()
                            .any(|current| current.activation == item.activation) =>
                    {
                        ContinuationSelection::RetainAccepted {
                            activation: item.activation.clone(),
                        }
                    }
                    Some(item) => ContinuationSelection::LeaveBlocked {
                        activation: item.activation.clone(),
                        blocker: blocker.clone(),
                    },
                    None => ContinuationSelection::LeaveUnmaterializedBlocked {
                        node_id: node.node_id.clone(),
                        blocker: blocker.clone(),
                    },
                },
            );
        }
        let plan = ContinuationPlan {
            source_epoch_id: source.clone(),
            epoch_id: input.activation.execution_epoch_id.clone(),
            selections,
            condition_runs: vec![review_condition_id()],
        };
        let digest = step_digest(&("host-review-unblock-v1", &plan.epoch_id))?;
        let events = [
            (
                format!("host-review-pause-{}", &digest[..48]),
                TurnContractEvent::PauseEpoch { epoch_id: source },
            ),
            (
                format!("host-review-continue-{}", &digest[..48]),
                TurnContractEvent::Continue { plan },
            ),
        ];
        let mut preview = contract.clone();
        for (command, event) in &events {
            let envelope = TurnContractEnvelope {
                schema_version: TURN_CONTRACT_SCHEMA_VERSION,
                command_id: CommandId::new(command.as_str()).map_err(error)?,
                expected_revision: preview.revision(),
                session_id: snapshot.owner().session_id.clone(),
                turn_id: snapshot.turn_id().clone(),
                event: event.clone(),
            };
            if let Err(refused) = preview.apply(&envelope) {
                return Ok(Err(format!(
                    "The reviewer was left blocked and the host could not start it: {refused}. \
                     Select the review and Continue, or Finish."
                )));
            }
        }
        for (command, event) in events {
            let appended = self.append(&command, event);
            self.fail_closed(appended)?;
        }
        self.changed.notify_waiters();
        Ok(Ok(()))
    }

    /// Start review round `round` in `epoch`: the reviewer's first
    /// activation, or a revision of its previous accepted one with the new
    /// result, or with the same result and its previous answer when the host
    /// asks again for a verdict. The driver runs it like any other admitted
    /// work. The reason when it cannot start.
    fn start_review_round(
        &mut self,
        snapshot: &DurableTurnSnapshot,
        reviewer: &GraphNode,
        previous: Option<&ContractActivation>,
        next: ReviewRound,
        criterion: &ReviewCriterion,
    ) -> Result<std::result::Result<(), String>> {
        let ReviewRound {
            repository,
            prompt,
            round,
            reask,
        } = next;
        let current = snapshot
            .contract()
            .epochs()
            .last()
            .ok_or_else(|| error("a reviewed turn has no epoch"))?;
        // A person's continuation that did not select the never-started
        // reviewer left it blocked in this epoch.
        let blocked = previous.is_none()
            && current.continuation.as_ref().is_some_and(|plan| {
                plan.selections.iter().any(|selection| {
                    matches!(selection, ContinuationSelection::LeaveUnmaterializedBlocked { node_id, .. }
                        if *node_id == reviewer.node_id)
                })
            });
        let epoch = if blocked {
            ExecutionEpochId::new(format!(
                "epoch-review-{}",
                &step_digest(&(
                    "host-review-unblock-epoch-v1",
                    snapshot.journal_id(),
                    &current.id
                ))?[..48]
            ))
            .map_err(error)?
        } else {
            current.id.clone()
        };
        let request = snapshot
            .request_ref()
            .ok_or_else(|| error("the turn request is unavailable"))?
            .clone();
        let round_text = self
            .content
            .retain_activation_evidence(ActivationEvidenceContent::Guidance {
                text: format!(
                    "This is review round {round} of {}. Judge only the result shown here.",
                    criterion.max_rounds
                ),
            })
            .map_err(error);
        let round_text = self.fail_closed(round_text)?.reference().clone();
        let digest = step_digest(&("host-review-round-v1", snapshot.journal_id(), &epoch, round))?;
        let activation = ActivationRef {
            session_id: snapshot.owner().session_id.clone(),
            turn_id: snapshot.turn_id().clone(),
            execution_epoch_id: epoch,
            node_id: reviewer.node_id.clone(),
            generation: round,
            activation_id: ActivationId::new(format!("review-activation-{}", &digest[..48]))
                .map_err(error)?,
        };
        let manifest_id =
            InputManifestId::new(format!("review-input-{}", &digest[..48])).map_err(error)?;
        let mut guidance = vec![request, prompt, round_text.clone()];
        guidance.extend(reask.as_ref().map(|(note, _)| note.clone()));
        let event = match previous {
            Some(previous) => {
                let mut input = previous.input.clone();
                input.manifest_id = manifest_id;
                input.activation = activation;
                input.guidance = guidance;
                input.repository = repository;
                // Asked again, the reviewer reads the answer that had no
                // verdict when it fits the room its request keeps for it; a
                // round about a new result starts clean.
                input.revision_context = match (&reask, &previous.output) {
                    (Some((_, true)), Some(output)) => Some(RevisionContext {
                        activation: previous.activation.clone(),
                        output: output.clone(),
                    }),
                    _ => None,
                };
                TurnContractEvent::ReviseAccepted {
                    previous: previous.activation.clone(),
                    input: Box::new(input),
                    invalidated_descendants: vec![],
                    evidence: reask.map_or(round_text, |(note, _)| note),
                }
            }
            None => {
                let (_, admission) = self
                    .content
                    .turn_admission(&self.canonical, &self.turn_id)
                    .map_err(error)?
                    .ok_or_else(|| error("a reviewed turn has no admission"))?;
                let admitted = admission
                    .nodes
                    .iter()
                    .find(|node| node.node_id == reviewer.node_id)
                    .ok_or_else(|| error("the reviewer has no admitted budget"))?;
                let input = ActivationInputManifest {
                    manifest_id,
                    activation,
                    definition: reviewer.definition.clone(),
                    conversation_id: reviewer.conversation_id.clone(),
                    starting_savepoint: reviewer.starting_savepoint.clone(),
                    parents: vec![],
                    guidance,
                    attachments: admitted.attachments.clone(),
                    repository,
                    budget: admitted.budget.clone(),
                    grant: Some(admitted.grant.clone()),
                    revision_context: None,
                };
                if blocked {
                    return self.unblock_review(snapshot, input, &round_text);
                }
                TurnContractEvent::StartActivation {
                    input: Box::new(input),
                }
            }
        };
        let appended = self.append(&format!("host-review-round-{}", &digest[..48]), event);
        self.fail_closed(appended)?;
        self.changed.notify_waiters();
        Ok(Ok(()))
    }

    /// One step of the review, once the required Agents are accepted and the
    /// required checks passed. Whether anything was recorded or started.
    fn drive_review(&mut self) -> Result<bool> {
        self.ready()?;
        // Set again below while a round waits to run again.
        self.review_rerun_at_ms = None;
        let snapshot = self.canonical.snapshot(&self.turn_id).map_err(error)?;
        let contract = snapshot.contract();
        if contract.state() != Some(LogicalTurnState::Running)
            || contract.stop_requested().is_some()
            || contract.has_unknown_effects()
            || contract
                .activations()
                .iter()
                .any(|item| item.state == ActivationState::Running)
            || self.execution_admission_closed
        {
            return Ok(false);
        }
        let Some(graph) = contract.graph() else {
            return Ok(false);
        };
        let Some(reviewer) = review_node(graph).cloned() else {
            return Ok(false);
        };
        let Some(criterion) = review_criterion(graph, &self.content).map_err(error)? else {
            return Ok(false);
        };
        let id = review_condition_id();
        if contract.current_condition(&id).is_some()
            || !super::turn_checks::epoch_runs(contract, &id)
        {
            return Ok(false);
        }
        let epoch = contract
            .epochs()
            .last()
            .ok_or_else(|| error("a reviewed turn has no epoch"))?
            .id
            .clone();
        // Every required Agent is accepted, and the checks passed on the
        // result.
        let accepted = contract.current_accepted_activations();
        if graph.nodes.iter().filter(|node| node.required).any(|node| {
            !accepted
                .iter()
                .any(|item| item.activation.node_id == node.node_id)
        }) {
            return Ok(false);
        }
        if let Some((group, _)) =
            group_of(graph).filter(|(group, _)| *group == CheckGroup::required())
        {
            let ready = ConditionId::new(group.ready_id()).map_err(error)?;
            if !contract.condition_satisfied(&ready) {
                return Ok(false);
            }
        }
        let scope = &graph
            .conditions
            .iter()
            .find(|condition| condition.condition_id == id)
            .ok_or_else(|| error("the review condition is missing"))?
            .nodes;
        let mut reviewed = Vec::with_capacity(scope.len());
        for node in scope {
            let Some(item) = accepted
                .iter()
                .find(|item| item.activation.node_id == *node)
            else {
                return Ok(false);
            };
            reviewed.push(*item);
        }
        let activations: Vec<_> = reviewed
            .iter()
            .map(|item| item.activation.clone())
            .collect();
        let candidate = match self.review_candidate(&snapshot, &reviewed)? {
            Ok(candidate) => candidate,
            Err(reason) => {
                let proof = serde_json::json!({"kind":"required_review","turn_id":snapshot.turn_id(),
                    "epoch_id":epoch,"activations":activations,"round":0,"max_rounds":criterion.max_rounds,
                    "verdict":ReviewVerdict::Unreadable,"findings":"","passed":false,"reason":reason});
                self.record_review(&epoch, activations, proof, false)?;
                return Ok(true);
            }
        };
        let prompt = self
            .content
            .retain_activation_evidence(ActivationEvidenceContent::Guidance {
                text: candidate.prompt.clone(),
            })
            .map_err(error);
        let prompt = self.fail_closed(prompt)?.reference().clone();
        let latest = contract
            .activations()
            .iter()
            .rev()
            .find(|item| item.activation.node_id == reviewer.node_id)
            .cloned();
        let repository = reviewed
            .first()
            .map(|item| item.input.repository.clone())
            .unwrap_or(RepositoryInput::Unavailable);
        let Some(latest) = latest else {
            let first = ReviewRound {
                repository,
                prompt,
                round: 1,
                reask: None,
            };
            if let Err(reason) =
                self.start_review_round(&snapshot, &reviewer, None, first, &criterion)?
            {
                let proof = serde_json::json!({"kind":"required_review","turn_id":snapshot.turn_id(),
                    "epoch_id":epoch,"activations":activations,"round":0,"max_rounds":criterion.max_rounds,
                    "verdict":ReviewVerdict::Unreadable,"findings":"","passed":false,"reason":reason,
                    "candidate_sha256":candidate.tree});
                self.record_review(&epoch, activations, proof, false)?;
            }
            return Ok(true);
        };
        let current_round = latest.activation.execution_epoch_id == epoch
            && latest.input.guidance.contains(&prompt);
        let next_round = latest
            .activation
            .generation
            .checked_add(1)
            .ok_or_else(|| error("review round overflow"));
        match latest.state {
            ActivationState::Running | ActivationState::Unstarted => Ok(false),
            ActivationState::Accepted if current_round => {
                let reask =
                    self.record_verdict(&snapshot, &criterion, &latest, candidate, activations)?;
                if let Some(note) = reask {
                    // The same prompt, so the next answer is bound to the
                    // same result.
                    let next = ReviewRound {
                        repository,
                        prompt,
                        round: next_round?,
                        reask: Some(note),
                    };
                    self.start_review_round(&snapshot, &reviewer, Some(&latest), next, &criterion)?
                        .map_err(error)?;
                }
                Ok(true)
            }
            ActivationState::Accepted => {
                let next = ReviewRound {
                    repository,
                    prompt,
                    round: next_round?,
                    reask: None,
                };
                self.start_review_round(&snapshot, &reviewer, Some(&latest), next, &criterion)?
                    .map_err(error)?;
                Ok(true)
            }
            ActivationState::Failed
            | ActivationState::Interrupted
            | ActivationState::Superseded => {
                // One transient provider failure of this round's model call,
                // or a model that did not finish in a way a fresh
                // conversation can, does not end the review: the round runs
                // again, once, after the provider's Retry-After or a backoff
                // for a transient failure.
                if current_round {
                    if let Some(at) = self.review_rerun_at(&snapshot, &criterion, &latest)? {
                        if now_ms()? < at {
                            // The driver waits until then; see
                            // `review_rerun_wait`.
                            self.review_rerun_at_ms = Some(at);
                            return Ok(false);
                        }
                        if self.rerun_review_round(&snapshot, &latest)? {
                            return Ok(true);
                        }
                    }
                }
                let cause = self
                    .reviewer_failure(&snapshot, &latest)?
                    .map(|cause| format!(": {cause}"))
                    .unwrap_or_default();
                let reason = if latest.activation.execution_epoch_id == epoch {
                    format!(
                        "The reviewer did not finish, so nothing approved this result{cause}. \
                         Select the reviewer and Continue to run it again, or Finish."
                    )
                } else {
                    format!(
                        "The reviewer did not finish in an earlier epoch and was not \
                         restarted{cause}. Select the reviewer and Continue to run it again, or \
                         Finish."
                    )
                };
                let proof = serde_json::json!({"kind":"required_review","turn_id":snapshot.turn_id(),
                    "epoch_id":epoch,"reviewer":latest.activation,"activations":activations,
                    "round":latest.activation.generation,"max_rounds":criterion.max_rounds,
                    "verdict":ReviewVerdict::Unreadable,"findings":"","passed":false,"reason":reason,
                    "candidate_sha256":candidate.tree});
                self.record_review(&epoch, activations, proof, false)?;
                Ok(true)
            }
        }
    }

    /// The failure the reviewer activation `failed` recorded, when its
    /// failure reference is that output: its text and when it was recorded.
    fn recorded_failure(
        &self,
        snapshot: &DurableTurnSnapshot,
        failed: &ContractActivation,
    ) -> Result<Option<(String, u64)>> {
        let Some(reservation) = self
            .content
            .activation_output_reservation(snapshot, &failed.activation)
            .map_err(error)?
        else {
            return Ok(None);
        };
        let Some(settled) = self
            .content
            .activation_output_settlement(&reservation)
            .map_err(error)?
        else {
            return Ok(None);
        };
        let output = &settled.content().output;
        Ok((failed.failure.as_ref() == Some(settled.reference()))
            .then(|| (output.text.clone(), output.recorded_at_unix_ms)))
    }

    /// When the review round the reviewer activation `failed` ended in runs
    /// again (Unix milliseconds), or `None` when it does not. It runs again
    /// once, and only while a round remains, as the host asks again for a
    /// verdict only while one remains: after the provider's `Retry-After`,
    /// or else [`review_rerun_backoff`], from when the failure was recorded,
    /// when the model call failed transiently (a 429, a 5xx, a timeout or a
    /// reset connection, as [`crate::provider_retry`] judges them); at once
    /// when the model did not finish in a way a fresh conversation can
    /// ([`rerun_cause_of_text`]). A cause the recorded failure does not
    /// state runs nothing again.
    fn review_rerun_at(
        &self,
        snapshot: &DurableTurnSnapshot,
        criterion: &ReviewCriterion,
        failed: &ContractActivation,
    ) -> Result<Option<u64>> {
        let reviewer: Vec<&ContractActivation> = snapshot
            .contract()
            .activations()
            .iter()
            .filter(|item| item.activation.node_id == failed.activation.node_id)
            .collect();
        if failed.state != ActivationState::Failed
            || failed.activation.generation >= criterion.max_rounds
            || reviewer
                .iter()
                .any(|item| crate::provider_retry::reruns_review_round(item, failed))
        {
            return Ok(None);
        }
        let Some((text, recorded_at)) = self.recorded_failure(snapshot, failed)? else {
            return Ok(None);
        };
        if let Some(failure) = crate::provider_retry::transient_failure_of_text(&text) {
            // The reviewer's rounds this turn already ran again.
            let earlier = reviewer
                .iter()
                .filter(|next| {
                    reviewer
                        .iter()
                        .any(|previous| crate::provider_retry::reruns_review_round(previous, next))
                })
                .count();
            let wait = match failure {
                crate::provider_retry::ProviderFailure::Status {
                    retry_after: Some(wait),
                    ..
                } => wait,
                _ => review_rerun_backoff(earlier),
            };
            let wait = u64::try_from(wait.as_millis()).map_err(error)?;
            return Ok(Some(recorded_at.saturating_add(wait)));
        }
        Ok(rerun_cause_of_text(&text).map(|_| recorded_at))
    }

    /// Why the reviewer activation `failed` did not finish, in words for the
    /// person, from the first line of its recorded failure: for a request
    /// larger than the reviewer's model takes, how many tokens it needed and
    /// the limit. `None` when it recorded none.
    fn reviewer_failure(
        &self,
        snapshot: &DurableTurnSnapshot,
        failed: &ContractActivation,
    ) -> Result<Option<String>> {
        Ok(self
            .recorded_failure(snapshot, failed)?
            .and_then(|(text, _)| failure_cause(&text)))
    }

    /// Run the review round `failed` ended in again: the next generation of
    /// the reviewer with the same input, so the same prompt about the same
    /// result. The reviewer is read-only, so nothing it did is undone.
    /// Whether it started; a run the contract would refuse is not started.
    fn rerun_review_round(
        &mut self,
        snapshot: &DurableTurnSnapshot,
        failed: &ContractActivation,
    ) -> Result<bool> {
        let digest = step_digest(&(
            "host-review-rerun-v1",
            snapshot.journal_id(),
            &failed.activation,
        ))?;
        let mut input = failed.input.clone();
        input.manifest_id =
            InputManifestId::new(format!("review-input-{}", &digest[..48])).map_err(error)?;
        input.activation.activation_id =
            ActivationId::new(format!("review-activation-{}", &digest[..48])).map_err(error)?;
        input.activation.generation = failed
            .activation
            .generation
            .checked_add(1)
            .ok_or_else(|| error("review round overflow"))?;
        let event = TurnContractEvent::StartActivation {
            input: Box::new(input),
        };
        let command = format!("host-review-rerun-{}", &digest[..48]);
        let mut preview = snapshot.contract().clone();
        let envelope = TurnContractEnvelope {
            schema_version: TURN_CONTRACT_SCHEMA_VERSION,
            command_id: CommandId::new(command.as_str()).map_err(error)?,
            expected_revision: preview.revision(),
            session_id: snapshot.owner().session_id.clone(),
            turn_id: snapshot.turn_id().clone(),
            event: event.clone(),
        };
        if preview.apply(&envelope).is_err() {
            return Ok(false);
        }
        let appended = self.append(&command, event);
        self.fail_closed(appended)?;
        self.changed.notify_waiters();
        Ok(true)
    }

    /// Record the verdict of the accepted reviewer `item`, which was shown
    /// `candidate`, and send a request for changes back to the lead while
    /// rounds remain. An answer without a readable verdict about the tree it
    /// was shown records nothing while a round remains and the host has not
    /// yet asked this reviewer again about this result: the note to ask again
    /// with in the next round is returned instead, and whether that answer is
    /// shown beside it (when it fits [`reask_room`]).
    fn record_verdict(
        &mut self,
        snapshot: &DurableTurnSnapshot,
        criterion: &ReviewCriterion,
        item: &ContractActivation,
        candidate: ReviewCandidate,
        activations: Vec<ActivationRef>,
    ) -> Result<Option<(EvidenceRef, bool)>> {
        let epoch = item.activation.execution_epoch_id.clone();
        let round = item.activation.generation;
        let answer = self.accepted_answer(snapshot, &item.activation)?;
        let (verdict, findings) = parse_verdict(&answer);
        let seen = |phase| -> Result<Option<Option<String>>> {
            Ok(self
                .review_capture(snapshot, item, phase)?
                .map(|capture| capture.and_then(|capture| capture.tree_sha256)))
        };
        let before = seen(RepositorySnapshotPhase::Before)?;
        let after = seen(RepositorySnapshotPhase::After)?;
        let unbound = binding_failure(
            candidate.tree.as_deref(),
            before.as_ref().map(Option::as_deref),
            after.as_ref().map(Option::as_deref),
        );
        let mut asked_again = false;
        if verdict == ReviewVerdict::Unreadable && unbound.is_none() {
            let graph = snapshot
                .contract()
                .graph()
                .ok_or_else(|| error("a reviewed turn has no graph"))?;
            let (context, output) = self.reviewer_window(graph)?;
            let shown = review_tokens(&answer) <= reask_room(context, output);
            let mut notes = Vec::with_capacity(2);
            for text in [REASK, REASK_UNSHOWN] {
                let note = self
                    .content
                    .retain_activation_evidence(ActivationEvidenceContent::Guidance {
                        text: text.to_owned(),
                    })
                    .map_err(error);
                notes.push(self.fail_closed(note)?.reference().clone());
            }
            asked_again = notes.iter().any(|note| item.input.guidance.contains(note));
            if !asked_again && round < criterion.max_rounds {
                let note = notes.swap_remove(if shown { 0 } else { 1 });
                return Ok(Some((note, shown)));
            }
        }
        let passed = verdict == ReviewVerdict::Approve && unbound.is_none();
        let mut plan = None;
        let reason = match (verdict, unbound) {
            (_, Some(reason)) => Some(reason.to_owned()),
            (ReviewVerdict::Unreadable, None) if asked_again => Some(UNREADABLE_AGAIN.to_owned()),
            (ReviewVerdict::Unreadable, None) => Some(UNREADABLE.to_owned()),
            (ReviewVerdict::Approve, None) => None,
            (ReviewVerdict::Changes, None) if round >= criterion.max_rounds => Some(format!(
                "The reviewer asked for changes in round {round} of {}, the last round the host \
                 runs. Read the findings, then Revise the lead with them, change the files \
                 yourself and Continue the review, or Finish.",
                criterion.max_rounds
            )),
            (ReviewVerdict::Changes, None) => {
                match self.review_revision(snapshot, criterion, round, &findings, &activations)? {
                    Ok(continuation) => {
                        plan = Some(continuation);
                        None
                    }
                    Err(reason) => Some(reason),
                }
            }
        };
        let proof = ReviewProof {
            kind: "required_review".into(),
            round,
            max_rounds: criterion.max_rounds,
            verdict,
            findings,
            passed,
            reason,
            candidate_sha256: candidate.tree.clone(),
            continued: plan.is_some(),
        };
        let mut value = serde_json::to_value(&proof).map_err(error)?;
        value["turn_id"] = serde_json::to_value(snapshot.turn_id()).map_err(error)?;
        value["epoch_id"] = serde_json::to_value(&epoch).map_err(error)?;
        value["reviewer"] = serde_json::to_value(&item.activation).map_err(error)?;
        value["activations"] = serde_json::to_value(&activations).map_err(error)?;
        value["reviewed_sha256"] = serde_json::to_value(before.flatten()).map_err(error)?;
        self.record_review(&epoch, activations, value, passed)?;
        let Some(plan) = plan else {
            return Ok(None);
        };
        // The host's own continuation: pause the epoch the review failed in
        // and continue in a new one that revises the lead.
        let digest = step_digest(&("host-review-continue-v1", &plan.epoch_id))?;
        let paused = self.append(
            &format!("host-review-pause-{}", &digest[..48]),
            TurnContractEvent::PauseEpoch {
                epoch_id: epoch.clone(),
            },
        );
        self.fail_closed(paused)?;
        let continued = self.append(
            &format!("host-review-continue-{}", &digest[..48]),
            TurnContractEvent::Continue { plan },
        );
        self.fail_closed(continued)?;
        self.changed.notify_waiters();
        Ok(None)
    }
}

impl SessionDispatchController {
    /// The turn driver's review step after the required checks: start the
    /// next round of the required reviewer, or record its verdict. Whether
    /// anything changed.
    pub(crate) fn drive_turn_review(&self) -> Result<bool> {
        self.lock()?.drive_review()
    }

    /// How long until the review round the last review step left waiting
    /// runs again, after a transient provider failure: `None` when none
    /// waits.
    pub(crate) fn review_rerun_wait(&self) -> Result<Option<std::time::Duration>> {
        let state = self.lock()?;
        let Some(at) = state.review_rerun_at_ms else {
            return Ok(None);
        };
        Ok(Some(std::time::Duration::from_millis(
            at.saturating_sub(now_ms()?),
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axocoatl_session::review_adjudication::{parse_adjudications, split_findings};

    /// Every reviewer prompt asks for findings numbered at the start of their
    /// line, beside the verdict form 1.2 reads.
    #[test]
    fn the_reviewer_prompt_carries_the_numbering_rule() {
        let prompt = reviewer_prompt_head(Some("tree-1"), false);
        assert!(
            prompt.contains("VERDICT: APPROVE\nVERDICT: CHANGES\n"),
            "{prompt}"
        );
        assert!(prompt.contains(FINDING_ID_RULE), "{prompt}");
        assert!(prompt.contains("Number each finding F1, F2, ... at the start of its line"));
        assert!(
            prompt.ends_with("Repository tree reviewed: tree-1\n"),
            "{prompt}"
        );
        assert!(
            reviewer_prompt_head(None, false).contains("Repository tree reviewed: not captured")
        );
        assert!(!prompt.contains(PARTIAL_CHANGE_RULE), "{prompt}");
        // The example the rule gives is itself a finding the host reads.
        let example = FINDING_ID_RULE
            .split('`')
            .nth(1)
            .expect("the rule shows an example");
        assert_eq!(split_findings(example).unwrap()[0].id, "F1");
    }

    /// The note that sends findings back asks for an ADJUDICATIONS block in
    /// the exact shape the host parses, before the findings it names.
    #[test]
    fn the_revision_note_carries_the_adjudications_instruction() {
        let findings = "F1: src/lib.rs:3: off by one\nF2: src/api.rs:9: unchecked input";
        let note = revision_note("reviewer", 1, 2, findings);
        assert!(
            note.starts_with(
                "The required reviewer (reviewer) asked for changes in review round 1 of 2. \
             Answer every finding below in an ADJUDICATIONS block: a fenced JSON array of \
             {\"id\":\"F1\",\"decision\":\"accept\"|\"reject\",\"reason\":\"...\"}, one entry \
             per finding, then fix each finding you accept.\n\n"
            ),
            "{note}"
        );
        assert!(note.contains(findings), "{note}");
        assert!(note.find(ADJUDICATION_RULE) < note.find(findings));
        assert!(!note.contains(UNNUMBERED));
        assert!(note.ends_with("again on your new result."), "{note}");
        // An answer in the shape the instruction gives is read.
        let answer = "Fixed.\n\nADJUDICATIONS\n```json\n[{\"id\":\"F1\",\"decision\":\"accept\",\
                      \"reason\":\"fixed\"},{\"id\":\"F2\",\"decision\":\"reject\",\"reason\":\
                      \"validated upstream\"}]\n```";
        assert_eq!(parse_adjudications(answer).unwrap().len(), 2);
        // Findings without ids are one finding, F1, and the note says so.
        let note = revision_note("reviewer", 2, 3, "src/lib.rs:3: no test");
        assert!(
            note.contains(&format!("{UNNUMBERED}\n\nsrc/lib.rs:3: no test")),
            "{note}"
        );
        assert!(note.contains("review round 2 of 3"));
    }

    /// The prompt and note additions leave 1.2's verdict reading as it was.
    #[test]
    fn verdict_parsing_is_unchanged_by_numbered_findings() {
        use axocoatl_session::turn_review::{parse_verdict, ReviewVerdict};
        assert_eq!(
            parse_verdict("VERDICT: CHANGES\nF1: src/lib.rs:3: off by one"),
            (
                ReviewVerdict::Changes,
                "F1: src/lib.rs:3: off by one".to_owned()
            )
        );
        assert_eq!(
            parse_verdict("**F1** src/lib.rs:3: x\nVERDICT: APPROVE").0,
            ReviewVerdict::Approve
        );
        assert_eq!(
            parse_verdict("F1: VERDICT: CHANGES").0,
            ReviewVerdict::Unreadable
        );
    }

    /// `len` deterministic bytes no compressor shrinks.
    fn noise(len: usize, seed: u64) -> Vec<u8> {
        let mut state = seed | 1;
        let mut bytes = Vec::with_capacity(len + 8);
        while bytes.len() < len {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            bytes.extend_from_slice(&state.to_le_bytes());
        }
        bytes.truncate(len);
        bytes
    }

    /// The first 12 KiB of the patch a capture keeps of a change too large
    /// to record: the fix to src/lib.rs, then new files under gen/, as
    /// `git diff --binary` prints random ones (a binary patch in base85,
    /// or, for a file without a NUL byte, lines that are not UTF-8 and read
    /// as U+FFFD), as the capture's text prefix holds them.
    fn generated_prefix(size: usize) -> String {
        const BASE85: &[u8] =
            b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz!#$%&()*+-;<=>?@^_`{|}~";
        let mut prefix = "diff --git a/src/lib.rs b/src/lib.rs\nindex 384eba8..4dda3a8 100644\n\
             --- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -15,7 +15,7 @@ pub fn paginate<T: Clone>(items: \
             &[T], size: usize) -> Vec<Vec<T>> {\n         } else {\n-            items.len() % \
             size\n+            items.len() - start\n         };\n"
            .to_owned();
        for index in 0u64.. {
            if prefix.len() > 12 * 1024 {
                break;
            }
            let path = format!("gen/f{index}.bin");
            let data = noise(size, index + 1);
            if index % 4 == 0 {
                prefix.push_str(&format!(
                    "diff --git a/{path} b/{path}\nnew file mode 100644\nindex 0000000..3b7d2f0\n\
                     --- /dev/null\n+++ b/{path}\n@@ -0,0 +1,4 @@\n"
                ));
                for line in data.split(|byte| *byte == b'\n') {
                    prefix.push('+');
                    prefix.push_str(&String::from_utf8_lossy(line));
                    prefix.push('\n');
                }
            } else {
                prefix.push_str(&format!(
                    "diff --git a/{path} b/{path}\nnew file mode 100644\nindex \
                     {zero}..{digest}\nGIT binary patch\nliteral {size}\n",
                    zero = "0".repeat(40),
                    digest = axocoatl_session::receipt::sha256_hex(&data)[..40].to_owned(),
                ));
                for chunk in data.chunks(52) {
                    prefix.push('z');
                    for byte in noise(chunk.len() * 5 / 4, index) {
                        prefix.push(BASE85[byte as usize % BASE85.len()] as char);
                    }
                    prefix.push('\n');
                }
                prefix.push_str("\nliteral 0\nHcmV?d00001\n\n");
            }
        }
        let mut end = 12 * 1024;
        while !prefix.is_char_boundary(end) {
            end -= 1;
        }
        prefix.truncate(end);
        prefix
    }

    /// The lines of the paths that differ from HEAD of `names`.
    fn changed_lines<'a>(names: impl Iterator<Item = &'a [u8]>) -> String {
        names
            .map(|name| {
                format!(
                    "{}\t644\tfile\t{}\n",
                    base64::engine::general_purpose::STANDARD.encode(name),
                    "e".repeat(64)
                )
            })
            .collect()
    }

    /// The description of a change too large for one capture costs a few
    /// thousand tokens as the daemon counts a request (cl100k, which it
    /// fits every request to its model's context with; the reviewer reads
    /// it inside JSON, so both forms are counted), whatever the change: at
    /// most 5,000 for generated files beside a fix, and 9,000 for names and
    /// a patch chosen to cost the most. A reviewer with a 32,768-token
    /// context, which a request may fill to 27,852 with its output
    /// allowance, holds it beside the rest of its request (about 10,000
    /// tokens for `axocoatl run fix`). Listing every changed path up to
    /// 48 KiB instead would cost about 22,000 tokens for 3,072 generated
    /// files beside a fix, and that reviewer could not start.
    #[test]
    fn a_change_too_large_to_record_is_described_in_a_few_thousand_tokens() {
        use axocoatl_session::turn_review::{describe_partial_change, MAX_PARTIAL_CHANGE_BYTES};
        use axocoatl_token::{ApproximateCounter, TokenCounter};
        let counter = ApproximateCounter::new().unwrap();
        let tokens = |text: &str| {
            counter
                .count_text(text)
                .max(counter.count_text(&serde_json::to_string(text).unwrap()))
        };
        let names = |count: usize| -> Vec<Vec<u8>> {
            std::iter::once(b"src/lib.rs".to_vec())
                .chain((0..count).map(|index| format!("gen/f{index}.bin").into_bytes()))
                .collect()
        };
        // 3,072 files of 1 KiB, which a capture lists, and 12,288 of 256
        // bytes, more than the 10,000 paths it lists.
        let three = names(3072);
        let listed = changed_lines(three.iter().map(Vec::as_slice));
        let few = describe_partial_change(
            EarlierPaths::Listed(""),
            Some(&listed),
            &generated_prefix(1024),
            4_717_831,
        );
        let many = describe_partial_change(
            EarlierPaths::Listed(""),
            None,
            &generated_prefix(256),
            6_208_208,
        );
        // Without the lines of the capture taken when the turn began.
        let unknown = describe_partial_change(
            EarlierPaths::NotListed,
            Some(&listed),
            &generated_prefix(1024),
            4_717_831,
        );
        for (label, change) in [("3,072", &few), ("12,288", &many), ("unknown", &unknown)] {
            assert!(
                tokens(change) <= 5_000,
                "{label}: {} tokens",
                tokens(change)
            );
        }
        // Names and a patch chosen to cost the most tokens a byte: each name
        // quoted byte by byte, and a prefix of text that is not UTF-8.
        let costly: Vec<Vec<u8>> = (0..3072u32)
            .map(|index| {
                let mut name = format!("\u{85}{index}/").into_bytes();
                name.extend(
                    noise(60, u64::from(index) + 9)
                        .iter()
                        .map(|byte| byte | 0x80),
                );
                name
            })
            .collect();
        let lines = changed_lines(costly.iter().map(Vec::as_slice));
        let noisy = String::from_utf8_lossy(&noise(12 * 1024, 5)).into_owned();
        let worst =
            describe_partial_change(EarlierPaths::NotListed, Some(&lines), &noisy, 50_000_000);
        assert!(
            worst.len() <= MAX_PARTIAL_CHANGE_BYTES + 128,
            "{}",
            worst.len()
        );
        assert!(tokens(&worst) <= 9_000, "worst: {} tokens", tokens(&worst));
    }

    /// The reviewer is told, before the change, when what it is shown is not
    /// all of it, and that it has not seen the rest; a change shown whole
    /// adds nothing.
    #[test]
    fn the_reviewer_prompt_says_what_it_did_not_see() {
        let partial = reviewer_prompt_head(Some("tree-1"), true);
        assert!(
            partial.ends_with(&format!(
                "Approve only when nothing must change.\n\n{PARTIAL_CHANGE_RULE}\n\nRepository \
                 tree reviewed: tree-1\n"
            )),
            "{partial}"
        );
        for said in [
            "Not all of the change is shown below",
            "You have not seen that part.",
            "do not approve code you have not read",
        ] {
            assert!(PARTIAL_CHANGE_RULE.contains(said), "{said}");
        }
        let whole = reviewer_prompt_head(Some("tree-1"), false);
        assert!(!whole.contains("Not all of the change"), "{whole}");
        assert_eq!(whole.len() + PARTIAL_CHANGE_RULE.len() + 2, partial.len());
    }

    /// The fix to src/lib.rs the release gate's writer makes.
    const GATE_FIX: &str = "diff --git a/src/lib.rs b/src/lib.rs\nindex 384eba8..4dda3a8 100644\n\
        --- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -15,7 +15,7 @@ pub fn paginate<T: Clone>(items: \
        &[T], size: usize) -> Vec<Vec<T>> {\n         let length = if page + 1 < count {\n      \
               size\n         } else {\n-            items.len() % size\n+            \
        items.len() - start\n         };\n         pages.push(items[start..start + \
        length].to_vec());\n     }\n";

    /// A new file of `size` random bytes as `git diff --binary` prints it:
    /// `literal`, then a line of base85 per 52 bytes (they do not compress).
    fn binary_patch(path: &str, size: usize, seed: u64) -> String {
        const BASE85: &[u8] =
            b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz!#$%&()*+-;<=>?@^_`{|}~";
        let mut patch = format!(
            "diff --git a/{path} b/{path}\nnew file mode 100755\nindex \
             0000000000000000000000000000000000000000..{}\nGIT binary patch\nliteral {size}\n",
            "7f4b1e46a3b60975bbfb9e59a1995d3237fb8d69"
        );
        for chunk in noise(size, seed).chunks(52) {
            patch.push(if chunk.len() == 52 { 'z' } else { 'a' });
            for byte in noise(chunk.len().div_ceil(4) * 5, u64::from(chunk[0]) + seed) {
                patch.push(BASE85[byte as usize % BASE85.len()] as char);
            }
            patch.push('\n');
        }
        patch.push_str("\nliteral 0\nHcmV?d00001\n\n");
        patch
    }

    /// A new text file of `bytes` bytes of base64, as `base64` wraps it.
    fn base64_patch(path: &str, bytes: usize, seed: u64) -> String {
        use base64::Engine as _;
        let text = base64::engine::general_purpose::STANDARD.encode(noise(bytes / 4 * 3, seed));
        let lines: Vec<&str> = text
            .as_bytes()
            .chunks(76)
            .map(|line| std::str::from_utf8(line).unwrap())
            .collect();
        let mut patch = format!(
            "diff --git a/{path} b/{path}\nnew file mode 100644\nindex 0000000..3b7d2f0\n--- \
             /dev/null\n+++ b/{path}\n@@ -0,0 +1,{} @@\n",
            lines.len()
        );
        for line in lines {
            patch.push('+');
            patch.push_str(line);
            patch.push('\n');
        }
        patch
    }

    /// A new Rust file of about `bytes` bytes of real source: the start of
    /// the session crate's review module.
    fn rust_patch(path: &str, bytes: usize) -> String {
        let source = include_str!("../../axocoatl-session/src/turn_review.rs");
        let mut lines = Vec::new();
        let mut size = 0;
        for line in source.lines() {
            if size + line.len() + 2 > bytes {
                break;
            }
            size += line.len() + 2;
            lines.push(line);
        }
        let mut patch = format!(
            "diff --git a/{path} b/{path}\nnew file mode 100644\nindex 0000000..5c6d7e8\n--- \
             /dev/null\n+++ b/{path}\n@@ -0,0 +1,{} @@\n",
            lines.len()
        );
        for line in lines {
            patch.push('+');
            patch.push_str(line);
            patch.push('\n');
        }
        patch
    }

    /// A whole change, as the release gate's writers have left them beside
    /// the fix, is shown within what the reviewer's context holds beside the
    /// rest of its request as `axocoatl run fix` makes it, counted as the
    /// host counts a request: binary files named with their sizes and none
    /// of their data, 400 KB of base64 text left out and listed, and 30 KiB
    /// of Rust shown whole to a 32,768-token reviewer. Fitted to the
    /// smallest context onboarding configures, the Rust is left out and
    /// listed instead. In 1.4 and the 1.5.0 candidate a 24 KiB binary file
    /// was shown as its base85 data and that reviewer could not start.
    #[test]
    fn a_whole_change_is_shown_within_the_reviewer_context() {
        use axocoatl_session::turn_review::MAX_CHANGE_BYTES;
        let instructions = axocoatl_config::loadout::builtin_loadouts()
            .into_iter()
            .map(std::result::Result::unwrap)
            .find(|loadout| loadout.file.id == "fix")
            .and_then(|loadout| loadout.file.review)
            .and_then(|review| review.instructions)
            .unwrap();
        let head = reviewer_prompt_head(Some(&"0".repeat(64)), true);
        let answers = "\n## Final answer of writer\nFixed src/lib.rs: the last page is now sized \
                       by the items left after its start, so it is full when the total is a \
                       multiple of the page size.\n";
        let task = "Paging skips the last item when the total is a multiple of the page size\n";
        let spent = |context: usize, output: usize| {
            review_tokens(&head)
                + review_tokens(answers)
                + review_tokens("\n## Change\n")
                + review_tokens(task)
                + review_tokens(&instructions)
                + reask_room(context, output)
                + REASK_ANSWER_FRAME
                + review_tokens(REASK_UNSHOWN)
        };
        let rust = rust_patch("src/pages.rs", 30 * 1024);
        assert!(rust.len() > 29 * 1024, "{}", rust.len());
        let scenarios = [
            (
                "one 24 KiB binary",
                format!("{GATE_FIX}{}", binary_patch("test_fix", 24 * 1024, 1)),
            ),
            (
                "one 200 KiB binary",
                format!("{GATE_FIX}{}", binary_patch("test_fix", 200 * 1024, 2)),
            ),
            (
                "400 KB of base64 text",
                format!("{GATE_FIX}{}", base64_patch("data.b64", 400_000, 3)),
            ),
            ("30 KiB of Rust", format!("{GATE_FIX}{rust}")),
        ];
        // The old bound alone kept 48 KiB of each, far past the context.
        assert!(review_tokens(&scenarios[0].1) > 20_000);
        for (context, output) in [(32_768, 8_192), FALLBACK_REVIEWER_WINDOW] {
            let limit = (context as f32 * axocoatl_token::COMPRESSION_TRIGGER_PCT) as usize;
            let spent = spent(context, output);
            let budget = change_budget(context, output, spent);
            for (label, patch) in &scenarios {
                assert!(
                    patch.len() <= 512 * 1024,
                    "{label}: a capture records it whole"
                );
                let change = describe_change(
                    None,
                    patch,
                    true,
                    ChangeBudget {
                        tokens: budget,
                        count: &review_tokens,
                    },
                );
                let text = &change.text;
                let tokens = review_tokens(text);
                eprintln!(
                    "{context}: {label}: patch {} bytes, {} tokens; shown {} bytes, {tokens} \
                     tokens of a budget of {budget}; the rest of the prompt {spent}",
                    patch.len(),
                    review_tokens(patch),
                    text.len()
                );
                assert!(tokens <= budget, "{label}: {tokens} > {budget}");
                assert!(
                    spent + tokens + REVIEW_REQUEST_RESERVE + output <= limit,
                    "{label}"
                );
                assert!(text.contains(GATE_FIX), "{label}: the fix is shown: {text}");
                assert!(!text.contains("\nz") && !text.contains("HcmV"), "{label}");
                assert!(text.len() <= MAX_CHANGE_BYTES + 16 * 1024, "{label}");
            }
        }
        let shown = |context: usize, output: usize, patch: &str| {
            describe_change(
                None,
                patch,
                true,
                ChangeBudget {
                    tokens: change_budget(context, output, spent(context, output)),
                    count: &review_tokens,
                },
            )
        };
        // Binary files: named with their sizes, nothing else left out.
        for (index, size) in [(0, 24 * 1024), (1, 200 * 1024)] {
            let change = shown(32_768, 8_192, &scenarios[index].1);
            assert!(change.partial);
            assert!(
                change.text.contains(&format!(
                    "GIT binary patch\n[binary data left out: the file's new content is {size} \
                     bytes]\n"
                )),
                "{}",
                change.text
            );
            assert!(!change.text.contains("Not all of the change"));
        }
        // Dense text: left out and listed with its bytes, the fix shown.
        let change = shown(32_768, 8_192, &scenarios[2].1);
        assert!(change.partial);
        assert!(
            change
                .text
                .starts_with("[Not all of the change is shown: its patch is "),
            "{}",
            change.text
        );
        assert!(
            change.text.contains("\ndata.b64: left out, "),
            "{}",
            change.text
        );
        // 30 KiB of Rust: whole, exactly as the patch has it.
        assert_eq!(
            shown(32_768, 8_192, &scenarios[3].1),
            DescribedChange {
                text: scenarios[3].1.clone(),
                partial: false
            }
        );
        let (context, output) = FALLBACK_REVIEWER_WINDOW;
        let change = shown(context, output, &scenarios[3].1);
        assert!(change.partial);
        assert!(
            change.text.contains("\nsrc/pages.rs: left out, "),
            "{}",
            change.text
        );
    }

    /// A re-ask shows the previous answer only within a sixteenth of what a
    /// request may hold beside the output allowance, at most 2,048 tokens,
    /// and every round's request keeps that room.
    #[test]
    fn a_reask_keeps_a_bounded_room_for_the_previous_answer() {
        // 32,768 tokens: (27,852 - 8,192) / 16.
        assert_eq!(reask_room(32_768, 8_192), 1_228);
        let (context, output) = FALLBACK_REVIEWER_WINDOW;
        assert_eq!(reask_room(context, output), 358);
        assert_eq!(reask_room(262_144, 8_192), MAX_REASK_ANSWER_TOKENS);
        assert_eq!(reask_room(4_096, 8_192), 0);
    }

    /// Only a failure a fresh conversation can do better than runs a review
    /// round again at once: a model that kept calling tools and gave no
    /// answer, used every tool round, or reached its token limit for the
    /// activation. The same request's context overflow, a used-up Session
    /// budget, a refusal or a provider's failure does not.
    #[test]
    fn only_a_reviewer_a_fresh_conversation_can_help_runs_again_at_once() {
        let failed = |line: &str| format!("Activation failed: {line}\n\nmore detail");
        for (line, cause) in [
            (
                "Tool call failed: agent tool loop - the model was asked for its final answer \
                 (your last 4 tool rounds repeated the same calls); it called a tool instead \
                 (read_file) and wrote no answer. Retry with a more capable model or narrow the \
                 task",
                "its model kept calling tools and gave no answer",
            ),
            (
                "Tool call failed: agent tool loop - the model returned no final answer after 3 \
                 tool calls, including 1 failed and 0 unresolved calls",
                "its model kept calling tools and gave no answer",
            ),
            (
                "LLM provider error: Streaming error: native Ollama: native response has no \
                 final content or tool call",
                "its model gave no answer and called no tool it was offered",
            ),
            (
                "This Agent reached its tool-round limit for this activation (128 rounds) and \
                 still asked for read_file; those calls did not run. Run it again to go on, or \
                 narrow the task.",
                "it used every tool round of its activation",
            ),
            (
                "This Agent reached its token limit for this activation (16,000 tokens; 17,000 \
                 needed). Raise the Agent's token budget or narrow the task.",
                "it reached its token limit for the activation",
            ),
        ] {
            assert_eq!(rerun_cause_of_text(&failed(line)), Some(cause), "{line}");
        }
        for text in [
            failed("Current request needs 34440 context tokens, exceeding limit 27852"),
            failed("The Session budget for this Agent is used up"),
            failed("LLM provider error: ollama API error: 502 - upstream failure"),
            failed("LLM provider error: Content filtered by ollama: safety"),
            failed("LLM provider error: Streaming error: native Ollama: invalid JSON line"),
            "Tool call failed: agent tool loop - no host line".to_owned(),
            String::new(),
        ] {
            assert_eq!(rerun_cause_of_text(&text), None, "{text}");
        }
        // The native Ollama provider words an empty response as the host
        // reads it.
        let (_, message) = NATIVE_NO_ANSWER.split_once("native Ollama: ").unwrap();
        assert!(
            include_str!("../../axocoatl-llm-ollama/src/native.rs")
                .contains(&format!("\"{message}\"")),
            "{message}"
        );
        assert_eq!(
            format!(
                "LLM provider error: {}",
                axocoatl_llm::ProviderError::Stream(format!("native Ollama: {message}"))
            ),
            NATIVE_NO_ANSWER
        );
    }

    /// A review's reason quotes the reviewer's own failure, and says in words
    /// how many tokens a request too large for its model needed and the
    /// limit.
    #[test]
    fn a_reviewer_failure_is_said_in_words() {
        assert_eq!(
            failure_cause(
                "Activation failed: Current request needs 34440 context tokens, exceeding limit \
                 27852\n\ndetail"
            )
            .as_deref(),
            Some(
                "its request needed 34,440 tokens of its model's context, more than the 27,852 \
                 one request may take with its output allowance, so the change, the Agents' \
                 answers and its instructions did not fit"
            )
        );
        assert_eq!(
            failure_cause("Activation failed: LLM provider error: ollama API error: 503 - x.")
                .as_deref(),
            Some("LLM provider error: ollama API error: 503 - x")
        );
        let long = failure_cause(&format!("Activation failed: {}", "é".repeat(400))).unwrap();
        assert!(long.len() <= MAX_FAILURE_CAUSE_BYTES + 3 && long.ends_with("..."));
        assert_eq!(failure_cause("not the host's line"), None);
        assert_eq!(failure_cause("Activation failed: "), None);
    }
}
