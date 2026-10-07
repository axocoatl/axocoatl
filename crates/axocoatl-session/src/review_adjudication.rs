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

use crate::run_outcome::{Adjudication, NodeObservation, ReviewFinding, ReviewRound};

/// Heading of the writer's adjudication block.
pub const ADJUDICATIONS_HEADING: &str = "ADJUDICATIONS";
/// Most findings one round may carry ids for.
pub const MAX_FINDINGS_PER_ROUND: usize = 64;
/// Longest reason kept per adjudication, in bytes.
pub const MAX_REASON_BYTES: usize = 2 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum AdjudicationError {
    #[error("adjudications: not implemented: {0}")]
    NotImplemented(&'static str),
    #[error("adjudications: {0}")]
    Invalid(String),
}

/// One adjudication as the writer wrote it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WrittenAdjudication {
    pub id: String,
    pub accept: bool,
    pub reason: String,
}

/// Split a round's findings text into findings with ids.
pub fn split_findings(_findings_text: &str) -> Result<Vec<ReviewFinding>, AdjudicationError> {
    Err(AdjudicationError::NotImplemented("split_findings"))
}

/// Read the `ADJUDICATIONS` block of a writer's answer.
pub fn parse_adjudications(_answer: &str) -> Result<Vec<WrittenAdjudication>, AdjudicationError> {
    Err(AdjudicationError::NotImplemented("parse_adjudications"))
}

/// Pair each round that sent findings back with the writer generation that
/// answered it, and return one adjudication per finding (missing ones
/// included).
pub fn adjudicate(
    _rounds: &[ReviewRound],
    _writer: &NodeObservation,
) -> Result<Vec<Adjudication>, AdjudicationError> {
    Err(AdjudicationError::NotImplemented("adjudicate"))
}
