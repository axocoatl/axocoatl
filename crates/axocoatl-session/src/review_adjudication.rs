//! Review findings with ids and the writer's adjudications of them.
//!
//! The reviewer numbers its findings `F1`, `F2`, ... (one per line starting
//! with the id). When the host sends findings back, the writer's next answer
//! carries an `ADJUDICATIONS` block: a fenced JSON array of
//! `{"id": "F1", "decision": "accept"|"reject", "reason": "..."}`. Every
//! finding of the round must be answered; an answer that leaves one out is
//! recorded as `missing` and the run needs attention.
//!
//! Models do not always fence what they are asked to fence, so a block is
//! read (here and for the QA explorer's and the audit's blocks) as a fenced
//! block after its heading, a JSON value under the heading without a fence,
//! or, with no heading at all, an answer that is exactly the expected JSON.
//!
//! Owner: workstream `review-qa`.

use serde_json::Value;

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
/// Why every finding of a round is missing when its block cannot be read:
/// the recorded reason starts with this, then says why (`...: the
/// ADJUDICATIONS block is not valid JSON: ...`).
pub const UNREADABLE_BLOCK: &str = "unreadable adjudications block";
/// Why every finding of a round is missing when the answer has no block.
pub const NO_BLOCK: &str = "the writer's answer has no ADJUDICATIONS block";

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AdjudicationError {
    /// The answer has no `ADJUDICATIONS` heading and is not itself a JSON
    /// array.
    #[error("adjudications: {NO_BLOCK}")]
    NoBlock,
    /// The block is not valid JSON or not a JSON array; the text says which.
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

/// The fence a line opens (three or more backticks or tildes).
fn fence(line: &str) -> Option<(char, usize)> {
    let trimmed = line.trim_start();
    let first = trimmed.chars().next()?;
    if first != '`' && first != '~' {
        return None;
    }
    let count = trimmed.chars().take_while(|c| *c == first).count();
    (count >= 3).then_some((first, count))
}

/// Whether `line` closes a fence opened with `count` `mark`s.
fn closes_fence(line: &str, mark: char, count: usize) -> bool {
    let trimmed = line.trim();
    trimmed.len() >= count && trimmed.chars().all(|c| c == mark)
}

/// Whether `line` is one of `headings`, and where on the line its block
/// starts. A heading is the word (any case; `_`, `-` or a space between
/// words) with optional Markdown heading, list or quote marks, emphasis, code
/// marks and a colon. Text after the colon starts its block
/// (`FINDINGS: []`); without such text the block starts on the next line.
fn heading_line(line: &str, headings: &[&str]) -> Option<(String, Option<usize>)> {
    let text = line.trim_start_matches(|c: char| {
        matches!(c, '#' | '*' | '_' | '>' | '-' | '`') || c.is_whitespace()
    });
    let lead = line.len() - text.len();
    let (head, inline) = match text.find(':') {
        Some(index) => (&text[..index], Some(lead + index + 1)),
        None => (text, None),
    };
    let head =
        head.trim_end_matches(|c: char| matches!(c, '*' | '_' | '#' | '`') || c.is_whitespace());
    if head.is_empty() || head.len() > 32 {
        return None;
    }
    let name = head.to_ascii_uppercase().replace([' ', '-'], "_");
    let name = headings.iter().find(|heading| **heading == name)?;
    let inline = inline.and_then(|at| {
        let rest = &line[at..];
        let start = at + rest.len()
            - rest
                .trim_start_matches(|c: char| c.is_whitespace() || matches!(c, '*' | '_'))
                .len();
        (start < line.len()).then_some(start)
    });
    Some(((*name).to_owned(), inline))
}

/// One heading line or fenced block of an answer.
struct Piece {
    /// Where its line starts in the answer.
    start: usize,
    kind: PieceKind,
}

enum PieceKind {
    /// `after`: where the heading's block starts in the answer.
    Heading {
        name: String,
        after: usize,
    },
    Fence(String),
}

/// The answer's `headings` lines and fenced blocks, in order. Lines inside a
/// fence are never headings; an unclosed fence runs to the end.
fn pieces(answer: &str, headings: &[&str]) -> Vec<Piece> {
    let mut lines = Vec::new();
    let mut offset = 0;
    for raw in answer.split_inclusive('\n') {
        lines.push((
            offset,
            offset + raw.len(),
            raw.trim_end_matches(['\n', '\r']),
        ));
        offset += raw.len();
    }
    let mut pieces = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        let (start, next, line) = lines[index];
        index += 1;
        if let Some((mark, count)) = fence(line) {
            let mut body = Vec::new();
            while index < lines.len() {
                let inner = lines[index].2;
                index += 1;
                if closes_fence(inner, mark, count) {
                    break;
                }
                body.push(inner);
            }
            pieces.push(Piece {
                start,
                kind: PieceKind::Fence(body.join("\n")),
            });
        } else if let Some((name, inline)) = heading_line(line, headings) {
            pieces.push(Piece {
                start,
                kind: PieceKind::Heading {
                    name,
                    after: inline.map_or(next, |at| start + at),
                },
            });
        }
    }
    pieces
}

/// The JSON object or array `text` starts with, as the text it spans, when
/// nothing but whitespace follows it on its last line (so `[1] I accept F1`
/// is text, not the array `[1]`); later lines are ignored. `None` when `text`
/// does not start with a valid one.
fn leading_json(text: &str) -> Option<&str> {
    if !text.starts_with(['[', '{']) {
        return None;
    }
    let mut values = serde_json::Deserializer::from_str(text).into_iter::<serde::de::IgnoredAny>();
    values.next()?.ok()?;
    let end = values.byte_offset();
    let line_rest = text[end..].split('\n').next().unwrap_or_default();
    line_rest.trim().is_empty().then(|| &text[..end])
}

/// The first JSON object or array ([`leading_json`]) that starts a line of
/// `answer[start..end]`; the value itself may run past `end`.
fn first_json_line(answer: &str, start: usize, end: usize) -> Option<&str> {
    let mut at = start;
    while at < end {
        let line = &answer[at..];
        let begin = at + line.len() - line.trim_start_matches([' ', '\t']).len();
        if let Some(value) = leading_json(&answer[begin..]) {
            return Some(value);
        }
        at += answer[at..end].find('\n')? + 1;
    }
    None
}

/// The whole answer as one JSON object or array, when it is exactly that
/// (surrounding whitespace aside).
pub(crate) fn whole_json(answer: &str) -> Option<Value> {
    let trimmed = answer.trim();
    if !trimmed.starts_with(['[', '{']) {
        return None;
    }
    serde_json::from_str(trimmed).ok()
}

/// Every fenced block's body, in order.
pub(crate) fn fenced_blocks(answer: &str) -> Vec<String> {
    pieces(answer, &[])
        .into_iter()
        .filter_map(|piece| match piece.kind {
            PieceKind::Fence(body) => Some(body),
            PieceKind::Heading { .. } => None,
        })
        .collect()
}

/// What follows a heading in a model's answer ([`headed_block`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HeadedBlock {
    /// No line of the answer is the heading.
    Absent,
    /// The heading is there with nothing after it.
    Empty,
    /// The block's text, to be read as its parser reads a block.
    Found(String),
}

/// The block of `heading` in `answer`. `headings` are the headings the
/// caller reads; a heading's block never reaches past the next of them.
///
/// After each line that is the heading ([`heading_line`]), the block is, in
/// order: an unfenced JSON object or array that starts a line before the
/// next heading or fence (on the heading's line after its colon, right below
/// it, or after a line of text; what follows the value on later lines is
/// ignored); else the fenced block that follows before the next heading;
/// else the text that follows (a value that starts like JSON up to the next
/// heading or fence, so its error is reported, otherwise the first
/// paragraph), which a parser that wants JSON then reports as not valid
/// JSON. Of several headings, the last with a JSON value or a fenced block
/// wins, else the last with any text.
pub(crate) fn headed_block(answer: &str, heading: &str, headings: &[&str]) -> HeadedBlock {
    let pieces = pieces(answer, headings);
    let mut seen = false;
    let mut block = None;
    let mut text = None;
    for (index, piece) in pieces.iter().enumerate() {
        let PieceKind::Heading { name, after } = &piece.kind else {
            continue;
        };
        if name != heading {
            continue;
        }
        seen = true;
        let rest = &answer[*after..];
        let start = after + rest.len() - rest.trim_start().len();
        let next = pieces.get(index + 1);
        let end = next.map_or(answer.len(), |piece| piece.start).max(start);
        if let Some(value) = first_json_line(answer, start, end) {
            block = Some(value.to_owned());
            continue;
        }
        if let Some(Piece {
            kind: PieceKind::Fence(body),
            ..
        }) = next
        {
            block = Some(body.clone());
            continue;
        }
        let following = answer[start..end].trim();
        let following = if following.starts_with(['[', '{']) {
            following.to_owned()
        } else {
            following
                .lines()
                .take_while(|line| !line.trim().is_empty())
                .collect::<Vec<_>>()
                .join("\n")
                .trim()
                .trim_matches(['*', '_'])
                .trim()
                .to_owned()
        };
        if !following.is_empty() {
            text = Some(following);
        }
    }
    match block.or(text) {
        Some(found) => HeadedBlock::Found(found),
        None if seen => HeadedBlock::Empty,
        None => HeadedBlock::Absent,
    }
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

/// Read the `ADJUDICATIONS` block of a writer's answer: the block after the
/// last `ADJUDICATIONS` heading, fenced or not (`headed_block`), or,
/// without the heading, the whole answer when it is exactly a JSON array. It
/// must be a JSON array; an entry without a readable `id` or a `decision` of
/// `accept` or `reject` is left out (its finding is then missing). Reasons
/// are kept to [`MAX_REASON_BYTES`].
pub fn parse_adjudications(answer: &str) -> Result<Vec<WrittenAdjudication>, AdjudicationError> {
    let value = match headed_block(answer, ADJUDICATIONS_HEADING, &[ADJUDICATIONS_HEADING]) {
        HeadedBlock::Found(block) => serde_json::from_str(block.trim()).map_err(|error| {
            AdjudicationError::Invalid(format!(
                "the {ADJUDICATIONS_HEADING} block is not valid JSON: {}",
                clip(&error.to_string(), 200)
            ))
        })?,
        HeadedBlock::Empty => {
            return Err(AdjudicationError::Invalid(format!(
                "the {ADJUDICATIONS_HEADING} block is not valid JSON: nothing follows the \
                 {ADJUDICATIONS_HEADING} heading"
            )))
        }
        HeadedBlock::Absent => match whole_json(answer) {
            Some(value @ Value::Array(_)) => value,
            _ => return Err(AdjudicationError::NoBlock),
        },
    };
    let Value::Array(entries) = value else {
        return Err(AdjudicationError::Invalid(format!(
            "the {ADJUDICATIONS_HEADING} block is not a JSON array"
        )));
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
            Err(AdjudicationError::Invalid(why)) => {
                report.adjudications.extend(missing_all(
                    format!("{UNREADABLE_BLOCK}: {why}"),
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

    fn invalid(answer: &str) -> String {
        match parse_adjudications(answer) {
            Err(AdjudicationError::Invalid(why)) => why,
            other => panic!("{answer:?} gave {other:?}"),
        }
    }

    #[test]
    fn a_missing_or_unreadable_block_is_an_error() {
        assert_eq!(
            parse_adjudications("No block.\n```json\n[]\n```"),
            Err(AdjudicationError::NoBlock)
        );
        assert_eq!(
            parse_adjudications("I fixed everything."),
            Err(AdjudicationError::NoBlock)
        );
        // A heading with something unreadable after it is not "no block".
        let why = invalid("ADJUDICATIONS\nnothing fenced");
        assert!(
            why.starts_with("the ADJUDICATIONS block is not valid JSON: "),
            "{why}"
        );
        let why = invalid("Done.\n\n## ADJUDICATIONS\n");
        assert_eq!(
            why,
            "the ADJUDICATIONS block is not valid JSON: nothing follows the ADJUDICATIONS heading"
        );
        let why = invalid("ADJUDICATIONS\n```json\n[{\"id\": \"F1\",]\n```");
        assert!(why.contains("not valid JSON"), "{why}");
        let why = invalid("ADJUDICATIONS\n```\n{\"id\": \"F1\"}\n```");
        assert_eq!(why, "the ADJUDICATIONS block is not a JSON array");
        // Unfenced JSON that does not parse is reported, not skipped.
        let why = invalid("ADJUDICATIONS:\n[{\"id\": \"F1\", \"decision\": accept}]");
        assert!(why.contains("not valid JSON"), "{why}");
        assert!(AdjudicationError::Invalid(why)
            .to_string()
            .starts_with("adjudications: unreadable adjudications block: the ADJUDICATIONS"));
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

    /// The writer's answers of the 1.3.0 smoke test's fix run 2, exactly as
    /// recorded: generation 2 answered review round 1 with an unfenced
    /// array after the heading, and both findings were recorded `missing`
    /// ("no ADJUDICATIONS block") although the writer had answered them.
    const FIX_RUN_2_GENERATION_1: &str =
        include_str!("../tests/fixtures/answers/fix-run2-writer-generation1.txt");
    const FIX_RUN_2_GENERATION_2: &str =
        include_str!("../tests/fixtures/answers/fix-run2-writer-generation2.txt");

    #[test]
    fn the_recorded_unfenced_answer_of_fix_run_2_answers_both_findings() {
        assert!(FIX_RUN_2_GENERATION_2.starts_with("ADJUDICATIONS:\n[\n  {\n"));
        let written = parse_adjudications(FIX_RUN_2_GENERATION_2).unwrap();
        let ids: Vec<_> = written.iter().map(|w| (w.id.as_str(), w.accept)).collect();
        assert_eq!(ids, [("F1", true), ("F2", true)]);
        assert!(written[1]
            .reason
            .starts_with("The fix correctly adds validation"));
        // Through the path the run took: round 1's two findings, answered by
        // generation 2.
        let rounds = [round(
            1,
            "F1: src/paginate.js:7: The function `pageCount` does not validate the `total` \
             argument.\nF2: src/paginate.js:21: The function `pageItems` does not enforce that \
             `page` is an integer.",
            true,
        )];
        let node = writer(&[Some(FIX_RUN_2_GENERATION_1), Some(FIX_RUN_2_GENERATION_2)]);
        let report = adjudicate_with_notes(&rounds, &node);
        let decisions: Vec<_> = report
            .adjudications
            .iter()
            .map(|a| (a.finding_id.as_str(), a.decision, a.writer_generation))
            .collect();
        assert_eq!(
            decisions,
            [
                ("F1", AdjudicationDecision::Accept, Some(2)),
                ("F2", AdjudicationDecision::Accept, Some(2)),
            ]
        );
        assert!(report.notes.is_empty(), "{:?}", report.notes);
        // Generation 1 put its array on the heading's line.
        let written = parse_adjudications(FIX_RUN_2_GENERATION_1).unwrap();
        assert_eq!(written.len(), 1);
        assert_eq!(written[0].id, "F1");
    }

    #[test]
    fn unfenced_blocks_and_whole_answers_are_read() {
        let entry = r#"{"id": "F1", "decision": "accept", "reason": "fixed"}"#;
        // Unfenced after the heading, with text and a diff after the array.
        let answer = format!(
            "**Adjudications:**\n\n[{entry}]\n\nHere is the change:\n```diff\n-a\n+b\n```\n"
        );
        assert_eq!(parse_adjudications(&answer).unwrap().len(), 1);
        // Text between the heading and the block, fenced or not.
        let answer = format!("ADJUDICATIONS\nBelow.\n\n```json\n[{entry}]\n```");
        assert_eq!(parse_adjudications(&answer).unwrap().len(), 1);
        let answer = format!("## ADJUDICATIONS\nHere they are:\n\n[\n  {entry}\n]\n");
        assert_eq!(parse_adjudications(&answer).unwrap().len(), 1);
        // A line that only starts like JSON is text, not the array `[1]`.
        let answer = format!("ADJUDICATIONS\n[1] I accept F1.\n```json\n[{entry}]\n```");
        assert_eq!(parse_adjudications(&answer).unwrap()[0].reason, "fixed");
        let why = invalid("ADJUDICATIONS\n[1] I accept F1.");
        assert!(why.contains("not valid JSON"), "{why}");
        // A later heading with a block wins over an earlier one; a later
        // heading with only text does not.
        let answer = format!(
            "ADJUDICATIONS: []\nOn reflection:\nADJUDICATIONS:\n[{entry}]\nADJUDICATIONS: see above"
        );
        assert_eq!(parse_adjudications(&answer).unwrap().len(), 1);
        // No heading: the whole answer when it is exactly a JSON array.
        assert_eq!(
            parse_adjudications(&format!("\n  [{entry}]  \n"))
                .unwrap()
                .len(),
            1
        );
        assert_eq!(parse_adjudications(entry), Err(AdjudicationError::NoBlock));
        assert_eq!(
            parse_adjudications(&format!("Answers: [{entry}]")),
            Err(AdjudicationError::NoBlock)
        );
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
            assert!(
                adjudication.reason.starts_with(&format!(
                    "{UNREADABLE_BLOCK}: the ADJUDICATIONS block is not valid JSON: "
                )),
                "{}",
                adjudication.reason
            );
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
