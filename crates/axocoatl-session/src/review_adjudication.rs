//! Review findings with ids and the writer's adjudications of them.
//!
//! The reviewer numbers its findings `F1`, `F2`, ... (one per line starting
//! with the id). When the host sends findings back, the writer's next answer
//! carries an `ADJUDICATIONS` block: a fenced JSON array of
//! `{"id": "F1", "decision": "accept"|"reject", "reason": "..."}`. Every
//! finding of the round must be answered; an answer that leaves one out is
//! recorded as `missing` and the run needs attention.
//!
//! Owner: workstream `review-qa`.

use crate::run_outcome::{
    Adjudication, AdjudicationDecision, NodeObservation, ReviewFinding, ReviewRound,
    ReviewVerdictKind,
};

/// Heading of the writer's adjudication block.
pub const ADJUDICATIONS_HEADING: &str = "ADJUDICATIONS";
/// Most findings one round may carry ids for.
pub const MAX_FINDINGS_PER_ROUND: usize = 64;
/// Longest reason kept per adjudication, in bytes.
pub const MAX_REASON_BYTES: usize = 2 * 1024;
/// Why every finding of a round is missing when its block is not JSON.
pub const UNREADABLE_BLOCK: &str = "unreadable adjudications block";
/// Why every finding of a round is missing when the answer has no block.
pub const NO_BLOCK: &str = "the writer's answer has no ADJUDICATIONS block";

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AdjudicationError {
    /// The answer has no fenced block after an `ADJUDICATIONS` heading.
    #[error("adjudications: {NO_BLOCK}")]
    NoBlock,
    /// The block is not a JSON array.
    #[error("adjudications: {UNREADABLE_BLOCK}: {0}")]
    Invalid(String),
}

/// One adjudication as the writer wrote it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WrittenAdjudication {
    pub id: String,
    pub accept: bool,
    pub reason: String,
}

/// The id a line starts with, `F<n>`, after list bullets, numbering,
/// Markdown emphasis or heading marks and an opening bracket, normalized
/// (`F01` is `F1`), with the rest of the line.
fn leading_id(line: &str) -> Option<(String, &str)> {
    let mut rest = line.trim_start();
    // Bullets and numbered-list markers: `-`, `*`, `+`, `•`, `1.`, `1)`.
    loop {
        let before = rest;
        rest = rest.trim_start_matches(['#', '>']).trim_start();
        if let Some(after) = rest
            .strip_prefix("- ")
            .or_else(|| rest.strip_prefix("+ "))
            .or_else(|| rest.strip_prefix("• "))
            .or_else(|| rest.strip_prefix("* "))
        {
            rest = after.trim_start();
        }
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        if digits > 0 && digits <= 3 {
            if let Some(after) = rest[digits..]
                .strip_prefix(". ")
                .or_else(|| rest[digits..].strip_prefix(") "))
            {
                rest = after.trim_start();
            }
        }
        rest = rest
            .trim_start_matches(['*', '_', '`', '[', '('])
            .trim_start();
        if rest == before {
            break;
        }
    }
    let after_f = rest.strip_prefix('F')?;
    let digits = after_f.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 || digits > 6 {
        return None;
    }
    let number: u32 = after_f[..digits].parse().ok()?;
    let tail = &after_f[digits..];
    // `F1` must end at a boundary: `F1:`, `F1.`, `F1 `, `**F1**`, `[F1]`.
    if tail
        .chars()
        .next()
        .is_some_and(|c| c.is_alphanumeric() || c == '_')
    {
        return None;
    }
    let text = tail.trim_start_matches(|c: char| {
        c.is_whitespace() || matches!(c, '*' | '_' | '`' | ']' | ')' | ':' | '.' | '-' | '—' | '–')
    });
    Some((format!("F{number}"), text))
}

/// Split a round's findings text into findings with ids. A line starting with
/// `F<n>` (optionally bulleted, numbered, bold or bracketed) starts a
/// finding; the lines until the next id belong to it. Text before the first
/// id is not a finding. Without any id the whole text is one finding `F1`.
/// A repeated id continues the earlier finding of that id. At most
/// [`MAX_FINDINGS_PER_ROUND`] findings are kept apart; the text of later ones
/// joins the last, with their ids, so nothing the reviewer wrote is lost.
pub fn split_findings(findings_text: &str) -> Result<Vec<ReviewFinding>, AdjudicationError> {
    let text = findings_text.trim();
    if text.is_empty() {
        return Ok(Vec::new());
    }
    let mut findings: Vec<ReviewFinding> = Vec::new();
    let mut current: Option<usize> = None;
    for line in text.lines() {
        match leading_id(line) {
            Some((id, rest)) => {
                let index = if let Some(index) = findings.iter().position(|f| f.id == id) {
                    index
                } else if findings.len() < MAX_FINDINGS_PER_ROUND {
                    findings.push(ReviewFinding {
                        id: id.clone(),
                        text: String::new(),
                    });
                    findings.len() - 1
                } else {
                    // Past the bound: the line joins the last kept finding
                    // with its own id, so it is still shown and sent back.
                    let last = findings.len() - 1;
                    push_line(&mut findings[last].text, line.trim());
                    current = Some(last);
                    continue;
                };
                if !rest.trim().is_empty() {
                    push_line(&mut findings[index].text, rest.trim_end());
                }
                current = Some(index);
            }
            None => {
                if let Some(index) = current {
                    push_line(&mut findings[index].text, line.trim_end());
                }
            }
        }
    }
    if findings.is_empty() {
        return Ok(vec![ReviewFinding {
            id: "F1".into(),
            text: text.to_owned(),
        }]);
    }
    for finding in &mut findings {
        finding.text = finding.text.trim().to_owned();
    }
    Ok(findings)
}

fn push_line(text: &mut String, line: &str) {
    if !text.is_empty() {
        text.push('\n');
    }
    text.push_str(line);
}

/// Whether `line` is the heading `heading`: the word alone on its line, with
/// optional Markdown heading marks, emphasis and a trailing colon.
fn is_heading(line: &str, heading: &str) -> bool {
    line.trim()
        .trim_start_matches('#')
        .trim()
        .trim_matches(|c: char| matches!(c, '*' | '_' | '`' | ':'))
        .trim()
        .eq_ignore_ascii_case(heading)
}

/// The fence a line opens or closes (three or more backticks or tildes).
fn fence(line: &str) -> Option<(char, usize)> {
    let trimmed = line.trim_start();
    let first = trimmed.chars().next()?;
    if first != '`' && first != '~' {
        return None;
    }
    let count = trimmed.chars().take_while(|c| *c == first).count();
    (count >= 3).then_some((first, count))
}

/// The body of each fenced block that is the first fenced block after a
/// `heading` line, in order. An unclosed block runs to the end. The QA
/// explorer's `FINDINGS` and `COVERAGE` blocks are read the same way.
pub(crate) fn fenced_blocks_after(answer: &str, heading: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut armed = false;
    let mut open: Option<((char, usize), Vec<&str>, bool)> = None;
    for line in answer.lines() {
        if let Some(((mark, count), body, keep)) = &mut open {
            if fence(line).is_some_and(|(m, c)| m == *mark && c >= *count)
                && line.trim().trim_start_matches(*mark).trim().is_empty()
            {
                if *keep {
                    blocks.push(body.join("\n"));
                }
                open = None;
            } else {
                body.push(line);
            }
            continue;
        }
        if let Some(found) = fence(line) {
            open = Some((found, Vec::new(), armed));
            armed = false;
        } else if is_heading(line, heading) {
            armed = true;
        }
    }
    if let Some((_, body, true)) = open {
        blocks.push(body.join("\n"));
    }
    blocks
}

/// At most `max` bytes of `text`, cut on a character boundary.
fn clip(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

/// `F1`, `f01`, ` F1 ` and `**F1**` all name `F1`.
fn normalize_id(id: &str) -> Option<String> {
    let trimmed = id
        .trim()
        .trim_matches(|c: char| matches!(c, '*' | '_' | '`' | '[' | ']' | '(' | ')'));
    let digits = trimmed
        .strip_prefix('F')
        .or_else(|| trimmed.strip_prefix('f'))?;
    if digits.is_empty() || digits.len() > 6 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(format!("F{}", digits.parse::<u32>().ok()?))
}

/// Read the `ADJUDICATIONS` block of a writer's answer: the last fenced block
/// that follows an `ADJUDICATIONS` heading. It must be a JSON array; an entry
/// without a readable `id` or a `decision` of `accept` or `reject` is left
/// out (its finding is then missing). Reasons are kept to
/// [`MAX_REASON_BYTES`].
pub fn parse_adjudications(answer: &str) -> Result<Vec<WrittenAdjudication>, AdjudicationError> {
    let block = fenced_blocks_after(answer, ADJUDICATIONS_HEADING)
        .pop()
        .ok_or(AdjudicationError::NoBlock)?;
    let value: serde_json::Value = serde_json::from_str(block.trim())
        .map_err(|error| AdjudicationError::Invalid(clip(&error.to_string(), 200)))?;
    let serde_json::Value::Array(entries) = value else {
        return Err(AdjudicationError::Invalid(
            "the block is not a JSON array".into(),
        ));
    };
    let mut written = Vec::with_capacity(entries.len());
    for entry in entries {
        let Some(id) = entry
            .get("id")
            .and_then(serde_json::Value::as_str)
            .and_then(normalize_id)
        else {
            continue;
        };
        let decision = entry
            .get("decision")
            .and_then(serde_json::Value::as_str)
            .map(|text| text.trim().to_ascii_lowercase());
        let accept = match decision.as_deref() {
            Some("accept" | "accepted" | "accepts") => true,
            Some("reject" | "rejected" | "rejects") => false,
            _ => continue,
        };
        let reason = entry
            .get("reason")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .trim();
        written.push(WrittenAdjudication {
            id,
            accept,
            reason: clip(reason, MAX_REASON_BYTES),
        });
    }
    Ok(written)
}

/// The adjudications of the writer's answers, and notes about answers that
/// were left out: ids no finding of the round has, and repeated ids.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AdjudicationReport {
    pub adjudications: Vec<Adjudication>,
    pub notes: Vec<String>,
}

/// The findings a round with `verdict` and `findings_text` lists: a request
/// for changes is always split ([`split_findings`]: unnumbered text is one
/// finding, `F1`); another verdict's text is split only when the reviewer
/// numbered it, so "Nothing must change." is not a finding.
pub fn findings_of(verdict: ReviewVerdictKind, findings_text: &str) -> Vec<ReviewFinding> {
    let split = split_findings(findings_text).unwrap_or_default();
    match (verdict, split.as_slice()) {
        (ReviewVerdictKind::Changes, _) => split,
        (_, [only]) if only.text == findings_text.trim() => Vec::new(),
        _ => split,
    }
}

/// The findings of `round`: as split already, or split from its text.
pub fn round_findings(round: &ReviewRound) -> Vec<ReviewFinding> {
    if round.findings.is_empty() {
        findings_of(round.verdict, &round.findings_text)
    } else {
        round.findings.clone()
    }
}

/// Pair each round that sent findings back with the writer generation that
/// answered it, and return one adjudication per finding (missing ones
/// included).
pub fn adjudicate(
    rounds: &[ReviewRound],
    writer: &NodeObservation,
) -> Result<Vec<Adjudication>, AdjudicationError> {
    Ok(adjudicate_with_notes(rounds, writer).adjudications)
}

/// As [`adjudicate`], with notes about ignored answers. Rounds whose
/// findings the host sent back (`continued`), in round order, are answered by
/// the writer's generations after its first, in generation order: each
/// sent-back round starts exactly one new writer generation. A round the host
/// did not send back (the last, or one that ended the turn) is not
/// adjudicated.
pub fn adjudicate_with_notes(
    rounds: &[ReviewRound],
    writer: &NodeObservation,
) -> AdjudicationReport {
    let mut generations: Vec<_> = writer.generations.iter().collect();
    generations.sort_by_key(|generation| generation.generation);
    let mut sent_back: Vec<&ReviewRound> = rounds.iter().filter(|round| round.continued).collect();
    sent_back.sort_by_key(|round| round.round);
    let mut report = AdjudicationReport::default();
    for (index, round) in sent_back.into_iter().enumerate() {
        let findings = round_findings(round);
        if findings.is_empty() {
            continue;
        }
        let answering = generations.get(index + 1).copied();
        let missing_all = |reason: String, generation: Option<u32>| {
            findings
                .iter()
                .map(|finding| Adjudication {
                    round: round.round,
                    finding_id: finding.id.clone(),
                    finding: finding.text.clone(),
                    decision: AdjudicationDecision::Missing,
                    reason: reason.clone(),
                    writer_generation: generation,
                })
                .collect::<Vec<_>>()
        };
        let Some(generation) = answering else {
            report.adjudications.extend(missing_all(
                format!(
                    "the writer did not run again after review round {}",
                    round.round
                ),
                None,
            ));
            continue;
        };
        let Some(answer) = generation.answer.as_deref() else {
            report.adjudications.extend(missing_all(
                format!(
                    "the writer's generation {} has no answer ({:?})",
                    generation.generation, generation.state
                )
                .to_lowercase(),
                Some(generation.generation),
            ));
            continue;
        };
        let written = match parse_adjudications(answer) {
            Ok(written) => written,
            Err(AdjudicationError::NoBlock) => {
                report
                    .adjudications
                    .extend(missing_all(NO_BLOCK.into(), Some(generation.generation)));
                continue;
            }
            Err(AdjudicationError::Invalid(_)) => {
                report.adjudications.extend(missing_all(
                    UNREADABLE_BLOCK.into(),
                    Some(generation.generation),
                ));
                continue;
            }
        };
        let mut used = vec![false; written.len()];
        for finding in &findings {
            let found = written.iter().position(|entry| entry.id == finding.id);
            let (decision, reason) = match found {
                Some(position) => {
                    used[position] = true;
                    let entry = &written[position];
                    let decision = if entry.accept {
                        AdjudicationDecision::Accept
                    } else {
                        AdjudicationDecision::Reject
                    };
                    if entry.reason.is_empty() {
                        (
                            AdjudicationDecision::Missing,
                            format!(
                                "the writer answered {} to {} without a reason",
                                if entry.accept { "accept" } else { "reject" },
                                finding.id
                            ),
                        )
                    } else {
                        (decision, entry.reason.clone())
                    }
                }
                None => (
                    AdjudicationDecision::Missing,
                    format!("the writer did not answer {}", finding.id),
                ),
            };
            report.adjudications.push(Adjudication {
                round: round.round,
                finding_id: finding.id.clone(),
                finding: finding.text.clone(),
                decision,
                reason,
                writer_generation: Some(generation.generation),
            });
        }
        for (entry, used) in written.iter().zip(used) {
            if used {
                continue;
            }
            let note = if findings.iter().any(|finding| finding.id == entry.id) {
                format!(
                    "Review round {}: the writer answered {} more than once; the first answer counts.",
                    round.round, entry.id
                )
            } else {
                format!(
                    "Review round {}: the writer answered {}, which the round has no finding for; the answer was ignored.",
                    round.round, entry.id
                )
            };
            if !report.notes.contains(&note) {
                report.notes.push(note);
            }
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run_outcome::{GenerationObservation, ModelIdentity, NodeState, ReviewVerdictKind};

    fn ids(findings: &[ReviewFinding]) -> Vec<&str> {
        findings.iter().map(|finding| finding.id.as_str()).collect()
    }

    #[test]
    fn ids_start_findings_with_bullets_bold_and_numbering() {
        let text = "Two defects.\n\
            - **F1**: src/lib.rs:3: off by one\n  the loop skips the last item\n\
            * F2. src/api.rs:9: missing check\n\
            3. [F3] docs/README.md:1: stale example\n\
            ## F4 - src/x.rs:1: panics on empty input\n\
            F5) tests are missing";
        let findings = split_findings(text).unwrap();
        assert_eq!(ids(&findings), ["F1", "F2", "F3", "F4", "F5"]);
        assert_eq!(
            findings[0].text,
            "src/lib.rs:3: off by one\n  the loop skips the last item"
        );
        assert_eq!(findings[1].text, "src/api.rs:9: missing check");
        assert_eq!(findings[2].text, "docs/README.md:1: stale example");
        assert_eq!(findings[3].text, "src/x.rs:1: panics on empty input");
        assert_eq!(findings[4].text, "tests are missing");
    }

    #[test]
    fn words_that_only_start_with_f_are_not_ids() {
        let text = "F1: a\nFoo bar\nF2b is not an id\nFF3 neither\n**F2** b";
        let findings = split_findings(text).unwrap();
        assert_eq!(ids(&findings), ["F1", "F2"]);
        assert_eq!(
            findings[0].text,
            "a\nFoo bar\nF2b is not an id\nFF3 neither"
        );
        assert_eq!(findings[1].text, "b");
    }

    #[test]
    fn text_without_ids_is_one_finding() {
        let text = "src/lib.rs:3: off by one\nsrc/api.rs:9: missing check";
        assert_eq!(
            split_findings(text).unwrap(),
            vec![ReviewFinding {
                id: "F1".into(),
                text: text.into()
            }]
        );
        assert!(split_findings("  \n").unwrap().is_empty());
    }

    #[test]
    fn only_requests_for_changes_turn_unnumbered_text_into_a_finding() {
        use ReviewVerdictKind::*;
        assert_eq!(ids(&findings_of(Changes, "fix the loop")), ["F1"]);
        assert!(findings_of(Approve, "Nothing must change.").is_empty());
        assert!(findings_of(Unreadable, "Looks fine to me.").is_empty());
        assert_eq!(ids(&findings_of(Approve, "F1: a nit")), ["F1"]);
    }

    #[test]
    fn repeated_ids_continue_and_the_bound_keeps_every_line() {
        let findings = split_findings("F01: a\nF2: b\nF1: more about a").unwrap();
        assert_eq!(ids(&findings), ["F1", "F2"]);
        assert_eq!(findings[0].text, "a\nmore about a");
        let many: String = (1..=70).map(|n| format!("F{n}: item {n}\n")).collect();
        let findings = split_findings(&many).unwrap();
        assert_eq!(findings.len(), MAX_FINDINGS_PER_ROUND);
        let last = findings.last().unwrap();
        assert_eq!(last.id, "F64");
        assert!(last.text.contains("F70: item 70"), "{}", last.text);
    }

    #[test]
    fn the_last_block_after_the_heading_is_read() {
        let answer = "I fixed F1.\n\n## ADJUDICATIONS\n```json\n[{\"id\":\"F1\",\"decision\":\"reject\",\"reason\":\"old\"}]\n```\n\
            Then I reconsidered.\n\n**ADJUDICATIONS:**\n\n```json\n[\n {\"id\": \"F1\", \"decision\": \"accept\", \"reason\": \"fixed the loop\"},\n {\"id\": \"f02\", \"decision\": \"Reject\", \"reason\": \"intended\"},\n {\"id\": \"F3\", \"decision\": \"maybe\", \"reason\": \"?\"},\n {\"decision\": \"accept\"}\n]\n```\n\
            ```rust\nfn later() {}\n```\n";
        let written = parse_adjudications(answer).unwrap();
        assert_eq!(
            written,
            vec![
                WrittenAdjudication {
                    id: "F1".into(),
                    accept: true,
                    reason: "fixed the loop".into()
                },
                WrittenAdjudication {
                    id: "F2".into(),
                    accept: false,
                    reason: "intended".into()
                },
            ]
        );
    }

    #[test]
    fn a_missing_or_unreadable_block_is_an_error() {
        assert_eq!(
            parse_adjudications("No block.\n```json\n[]\n```"),
            Err(AdjudicationError::NoBlock)
        );
        assert_eq!(
            parse_adjudications("ADJUDICATIONS\nnothing fenced"),
            Err(AdjudicationError::NoBlock)
        );
        assert!(matches!(
            parse_adjudications("ADJUDICATIONS\n```json\n[{\"id\": \"F1\",]\n```"),
            Err(AdjudicationError::Invalid(_))
        ));
        assert!(matches!(
            parse_adjudications("ADJUDICATIONS\n```\n{\"id\": \"F1\"}\n```"),
            Err(AdjudicationError::Invalid(_))
        ));
        // An unclosed block still counts as the block.
        assert_eq!(
            parse_adjudications(
                "ADJUDICATIONS\n```json\n[{\"id\":\"F1\",\"decision\":\"accept\",\"reason\":\"ok\"}]"
            )
            .unwrap()
            .len(),
            1
        );
    }

    #[test]
    fn reasons_are_bounded() {
        let long = "é".repeat(MAX_REASON_BYTES);
        let answer = format!(
            "ADJUDICATIONS\n```json\n[{{\"id\":\"F1\",\"decision\":\"accept\",\"reason\":\"{long}\"}}]\n```"
        );
        let written = parse_adjudications(&answer).unwrap();
        assert!(written[0].reason.len() <= MAX_REASON_BYTES);
        assert!(written[0].reason.len() > MAX_REASON_BYTES - 4);
    }

    fn round(number: u32, text: &str, continued: bool) -> ReviewRound {
        ReviewRound {
            round: number,
            verdict: ReviewVerdictKind::Changes,
            passed: false,
            findings_text: text.into(),
            findings: Vec::new(),
            continued,
            candidate_sha256: None,
        }
    }

    fn writer(answers: &[Option<&str>]) -> NodeObservation {
        NodeObservation {
            node_id: "writer-node".into(),
            slot_id: "writer".into(),
            model: ModelIdentity {
                provider: "ollama".into(),
                model: "writer-model".into(),
                runtime: "native".into(),
            },
            required: true,
            kind: "slot".into(),
            generations: answers
                .iter()
                .enumerate()
                .map(|(index, answer)| GenerationObservation {
                    generation: index as u32 + 1,
                    state: if answer.is_some() {
                        NodeState::Accepted
                    } else {
                        NodeState::Failed
                    },
                    answer: answer.map(str::to_owned),
                    failure: None,
                })
                .collect(),
        }
    }

    fn block(entries: &[(&str, &str, &str)]) -> String {
        let entries: Vec<_> = entries
            .iter()
            .map(|(id, decision, reason)| {
                serde_json::json!({"id": id, "decision": decision, "reason": reason})
            })
            .collect();
        format!(
            "Done.\n\nADJUDICATIONS\n```json\n{}\n```\n",
            serde_json::to_string_pretty(&entries).unwrap()
        )
    }

    #[test]
    fn an_unreadable_block_leaves_every_finding_missing() {
        let rounds = [round(1, "F1: a\nF2: b", true)];
        let node = writer(&[
            Some("first"),
            Some("ADJUDICATIONS\n```json\n[{\"id\": \"F1\", oops]\n```"),
        ]);
        let adjudications = adjudicate(&rounds, &node).unwrap();
        assert_eq!(adjudications.len(), 2);
        for adjudication in &adjudications {
            assert_eq!(adjudication.decision, AdjudicationDecision::Missing);
            assert_eq!(adjudication.reason, UNREADABLE_BLOCK);
            assert_eq!(adjudication.writer_generation, Some(2));
            assert_eq!(adjudication.round, 1);
        }
        // No block at all: missing, with that reason.
        let node = writer(&[Some("first"), Some("I fixed everything.")]);
        let adjudications = adjudicate(&rounds, &node).unwrap();
        assert!(adjudications
            .iter()
            .all(|a| a.decision == AdjudicationDecision::Missing && a.reason == NO_BLOCK));
    }

    #[test]
    fn a_partial_answer_leaves_missing_entries_and_notes_unknown_ids() {
        let rounds = [round(1, "F1: a\nF2: b\nF3: c", true)];
        let answer = block(&[
            ("F1", "accept", "fixed"),
            ("F3", "reject", "intended behavior"),
            ("F3", "accept", "second answer"),
            ("F9", "accept", "no such finding"),
            ("F2", "accept", ""),
        ]);
        let node = writer(&[Some("first"), Some(&answer)]);
        let report = adjudicate_with_notes(&rounds, &node);
        let decisions: Vec<_> = report
            .adjudications
            .iter()
            .map(|a| (a.finding_id.as_str(), a.decision, a.reason.as_str()))
            .collect();
        assert_eq!(
            decisions,
            vec![
                ("F1", AdjudicationDecision::Accept, "fixed"),
                (
                    "F2",
                    AdjudicationDecision::Missing,
                    "the writer answered accept to F2 without a reason"
                ),
                ("F3", AdjudicationDecision::Reject, "intended behavior"),
            ]
        );
        assert_eq!(report.adjudications[0].finding, "a");
        assert_eq!(report.notes.len(), 2, "{:?}", report.notes);
        assert!(report.notes[0].contains("F3 more than once"));
        assert!(report.notes[1].contains("F9"));
        // An answer that names none of them: every finding missing.
        let node = writer(&[Some("first"), Some(&block(&[]))]);
        let adjudications = adjudicate(&rounds, &node).unwrap();
        assert_eq!(adjudications.len(), 3);
        assert_eq!(adjudications[1].reason, "the writer did not answer F2");
    }

    #[test]
    fn two_rounds_are_answered_by_the_next_generations_and_the_last_round_only_if_sent_back() {
        let rounds = [
            round(1, "F1: a", true),
            round(2, "F1: still a\nF2: new b", true),
            round(3, "F1: final", false),
        ];
        let node = writer(&[
            Some("first answer"),
            Some(&block(&[("F1", "accept", "fixed a")])),
            Some(&block(&[
                ("F1", "reject", "a is fixed; the reviewer read old code"),
                ("F2", "accept", "fixed b"),
            ])),
        ]);
        let adjudications = adjudicate(&rounds, &node).unwrap();
        let seen: Vec<_> = adjudications
            .iter()
            .map(|a| {
                (
                    a.round,
                    a.finding_id.as_str(),
                    a.decision,
                    a.writer_generation,
                )
            })
            .collect();
        assert_eq!(
            seen,
            vec![
                (1, "F1", AdjudicationDecision::Accept, Some(2)),
                (2, "F1", AdjudicationDecision::Reject, Some(3)),
                (2, "F2", AdjudicationDecision::Accept, Some(3)),
            ]
        );
        // A writer that never ran again for round 2: its findings are missing.
        let node = writer(&[
            Some("first answer"),
            Some(&block(&[("F1", "accept", "fixed a")])),
        ]);
        let adjudications = adjudicate(&rounds, &node).unwrap();
        assert_eq!(adjudications.len(), 3);
        assert!(adjudications[1..]
            .iter()
            .all(|a| a.decision == AdjudicationDecision::Missing
                && a.writer_generation.is_none()
                && a.reason.contains("did not run again")));
        // A generation that failed without an answer.
        let node = writer(&[Some("first answer"), None]);
        let adjudications = adjudicate(&rounds[..1], &node).unwrap();
        assert_eq!(adjudications[0].decision, AdjudicationDecision::Missing);
        assert_eq!(adjudications[0].writer_generation, Some(2));
    }

    #[test]
    fn rounds_with_split_findings_use_them() {
        let mut first = round(1, "ignored text", true);
        first.findings = vec![ReviewFinding {
            id: "F7".into(),
            text: "kept".into(),
        }];
        let node = writer(&[Some("a"), Some(&block(&[("F7", "accept", "ok")]))]);
        let adjudications = adjudicate(&[first], &node).unwrap();
        assert_eq!(adjudications.len(), 1);
        assert_eq!(adjudications[0].finding_id, "F7");
        assert_eq!(adjudications[0].decision, AdjudicationDecision::Accept);
    }
}
