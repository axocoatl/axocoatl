//! The fix loadout: one turn of one writer with required checks and a
//! required review by a different model; adjudications from the writer's
//! answers. Owner: review-qa.
//!
//! The turn runs on the 1.2 review machinery: the host runs the checks on
//! the exact candidate, starts the reviewer only after they pass, and sends a
//! request for changes back to the writer while rounds remain. The reviewer
//! numbers its findings (`F1`, `F2`, ...) and the writer answers each one in
//! an `ADJUDICATIONS` block (`session_dispatch_turn_review.rs`). After the
//! turn this driver splits each round's findings by id, pairs every round the
//! host sent back with the writer's answer, and warns (never refuses) when the
//! reviewer runs the writer's model.

use async_trait::async_trait;
use axocoatl_config::loadout::{LoadoutRole, ResolvedLoadout};
use axocoatl_session::review_adjudication::{adjudicate_with_notes, findings_of};
use axocoatl_session::run_outcome::{
    same_model_warning, Adjudication, AdjudicationDecision, ModelIdentity, NodeObservation,
    RunTurnRef, RunWarning, TurnObservation, SAME_MODEL_REVIEWER,
};
use axocoatl_session::run_record::RunEvent;

use super::{KindDriver, KindReport, RunContext, RunError, RunHost};

/// Warning code: an answer in the writer's `ADJUDICATIONS` block was ignored
/// (an id the round has no finding for, or a second answer to one).
pub const IGNORED_ADJUDICATION: &str = "ignored_adjudication";

pub struct FixDriver;

#[async_trait]
impl KindDriver for FixDriver {
    async fn drive(&self, host: &dyn RunHost, run: &RunContext) -> Result<KindReport, RunError> {
        let turn = super::driver::run_single_turn(host, run).await?;
        let report = fix_report(run, turn);
        record_report(host, run, &report).await?;
        Ok(report)
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

/// The id of the loadout's writer Agent (its team slot).
fn writer_id(resolved: &ResolvedLoadout) -> Option<&str> {
    resolved
        .loadout
        .file
        .agents
        .iter()
        .find(|agent| agent.role == LoadoutRole::Writer)
        .map(|agent| agent.id.as_str())
}

/// The writer's node: its slot, or else the first required node that is not
/// the reviewer.
fn writer_node<'a>(
    resolved: &ResolvedLoadout,
    turn: &'a TurnObservation,
) -> Option<&'a NodeObservation> {
    let writer = writer_id(resolved);
    turn.nodes
        .iter()
        .find(|node| Some(node.slot_id.as_str()) == writer && node.kind != "reviewer")
        .or_else(|| {
            turn.nodes
                .iter()
                .find(|node| node.required && node.kind != "reviewer")
        })
}

/// The writer and reviewer identities the loadout resolved to, and the
/// identities the turn actually ran, each pair warned about once.
fn same_model_warnings(
    resolved: &ResolvedLoadout,
    turn: &TurnObservation,
    writer: Option<&NodeObservation>,
) -> Vec<RunWarning> {
    let mut warnings: Vec<RunWarning> = Vec::new();
    let mut push = |warning: Option<RunWarning>| {
        if let Some(warning) = warning {
            if !warnings
                .iter()
                .any(|known| known.message == warning.message)
            {
                warnings.push(warning);
            }
        }
    };
    if let Some(reviewer) = &resolved.reviewer_model {
        let writers: Vec<ModelIdentity> = resolved
            .loadout
            .file
            .agents
            .iter()
            .filter(|agent| agent.role == LoadoutRole::Writer)
            .filter_map(|agent| {
                let model = resolved.agent_models.get(&agent.id)?;
                Some(ModelIdentity {
                    provider: model.provider.clone(),
                    model: model.model.clone(),
                    runtime: serde_json::to_value(agent.runtime)
                        .ok()
                        .and_then(|value| value.as_str().map(str::to_owned))
                        .unwrap_or_else(|| "native".into()),
                })
            })
            .collect();
        let reviewer = ModelIdentity {
            provider: reviewer.provider.clone(),
            model: reviewer.model.clone(),
            runtime: "native".into(),
        };
        push(same_model_warning(&writers, &reviewer));
    }
    if let (Some(review), Some(writer)) = (&turn.review, writer) {
        push(same_model_warning(
            std::slice::from_ref(&writer.model),
            &review.reviewer,
        ));
    }
    warnings
}

/// What a finished fix turn shows beyond the turn itself: each round's
/// findings split by id, the writer's adjudications (missing ones included)
/// and the warnings.
pub fn fix_report(run: &RunContext, mut turn: TurnObservation) -> KindReport {
    let resolved = &run.resolved;
    if let Some(review) = turn.review.as_mut() {
        for round in &mut review.rounds {
            if round.findings.is_empty() {
                round.findings = findings_of(round.verdict, &round.findings_text);
            }
        }
    }
    let adjudicate = resolved
        .loadout
        .file
        .review
        .as_ref()
        .is_none_or(|review| review.adjudicate);
    let writer = writer_node(resolved, &turn);
    let mut warnings = same_model_warnings(resolved, &turn, writer);
    let mut adjudications: Vec<Adjudication> = Vec::new();
    if let (true, Some(review)) = (adjudicate, &turn.review) {
        match writer {
            Some(writer) => {
                let report = adjudicate_with_notes(&review.rounds, writer);
                adjudications = report.adjudications;
                warnings.extend(report.notes.into_iter().map(|message| RunWarning {
                    code: IGNORED_ADJUDICATION.into(),
                    message,
                }));
            }
            None => {
                // Findings were sent back, but the writer's answers were not
                // observed: every one of them is missing.
                for round in review.rounds.iter().filter(|round| round.continued) {
                    for finding in &round.findings {
                        adjudications.push(Adjudication {
                            round: round.round,
                            finding_id: finding.id.clone(),
                            finding: finding.text.clone(),
                            decision: AdjudicationDecision::Missing,
                            reason: "the writer's node was not observed in the turn".into(),
                            writer_generation: None,
                        });
                    }
                }
            }
        }
    }
    let turn_ref = RunTurnRef {
        turn_id: turn.turn_id.clone(),
        purpose: "run".into(),
        state: turn.state,
    };
    KindReport {
        turns: vec![turn],
        turn_refs: vec![turn_ref],
        adjudications,
        warnings,
        fail_on_findings: false,
        ..KindReport::default()
    }
}

/// Record what the fix turn adds to the run's events: a review phase line,
/// each adjudication, and each warning the loadout's resolution did not
/// already carry (core records those when the run is accepted).
async fn record_report(
    host: &dyn RunHost,
    run: &RunContext,
    report: &KindReport,
) -> Result<(), RunError> {
    let rounds = report
        .turns
        .first()
        .and_then(|turn| turn.review.as_ref())
        .map_or(0, |review| review.rounds.len());
    let missing = report
        .adjudications
        .iter()
        .filter(|item| item.decision == AdjudicationDecision::Missing)
        .count();
    host.record(
        &run.run_id,
        RunEvent::Phase {
            at_ms: now_ms(),
            phase: "review".into(),
            detail: format!(
                "{rounds} review round{}; {} adjudication{}, {missing} missing",
                if rounds == 1 { "" } else { "s" },
                report.adjudications.len(),
                if report.adjudications.len() == 1 {
                    ""
                } else {
                    "s"
                },
            ),
        },
    )
    .await?;
    for adjudication in &report.adjudications {
        host.record(
            &run.run_id,
            RunEvent::Adjudication {
                at_ms: now_ms(),
                adjudication: Box::new(adjudication.clone()),
            },
        )
        .await?;
    }
    let resolved_same_model = run
        .resolved
        .warnings
        .iter()
        .any(|warning| warning.code == SAME_MODEL_REVIEWER);
    for warning in &report.warnings {
        if warning.code == SAME_MODEL_REVIEWER && resolved_same_model {
            continue;
        }
        host.record(
            &run.run_id,
            RunEvent::Warning {
                at_ms: now_ms(),
                warning: warning.clone(),
            },
        )
        .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use axocoatl_session::run_outcome::{
        NodeState, ReviewOutcome, ReviewRound, ReviewVerdictKind, TurnState,
    };

    use super::super::qa::tests::{context, generation, node, turn, FakeHost};
    use super::*;

    const WRITER: &str = "openrouter:qwen/qwen3-coder";
    const OTHER: &str = "openrouter:openai/gpt-oss-120b";

    fn fix_context(writer: &str, reviewer: &str) -> RunContext {
        let repo = std::env::temp_dir();
        context(
            "fix",
            &[("writer_model", writer), ("reviewer_model", reviewer)],
            &repo,
        )
    }

    fn round(number: u32, verdict: ReviewVerdictKind, text: &str, continued: bool) -> ReviewRound {
        ReviewRound {
            round: number,
            verdict,
            passed: verdict == ReviewVerdictKind::Approve,
            findings_text: text.into(),
            findings: Vec::new(),
            continued,
            candidate_sha256: None,
        }
    }

    fn adjudications(entries: &[(&str, &str, &str)]) -> String {
        let entries: Vec<_> = entries
            .iter()
            .map(|(id, decision, reason)| {
                serde_json::json!({"id": id, "decision": decision, "reason": reason})
            })
            .collect();
        format!(
            "Changed src/cart.rs.\n\nADJUDICATIONS\n```json\n{}\n```",
            serde_json::to_string(&entries).unwrap()
        )
    }

    /// Two rounds of changes and an approval: the writer's second and third
    /// answers adjudicate rounds 1 and 2; the approving round is not
    /// adjudicated and its "Nothing must change." is not a finding.
    fn reviewed_turn(reviewer_model: &str, third: Option<&str>) -> TurnObservation {
        let second = adjudications(&[
            ("F1", "accept", "the loop now stops at len"),
            (
                "F2",
                "reject",
                "the input is validated by the caller in api.rs:40",
            ),
        ]);
        let mut writer_generations = vec![
            generation(1, NodeState::Superseded, Some("First answer."), None),
            generation(2, NodeState::Superseded, Some(&second), None),
        ];
        if let Some(third) = third {
            writer_generations.push(generation(3, NodeState::Accepted, Some(third), None));
        }
        let mut observed = turn(
            TurnState::Completed,
            vec![
                node("writer", "slot", "qwen/qwen3-coder", writer_generations),
                node("required-review", "reviewer", reviewer_model, vec![]),
            ],
        );
        observed.review = Some(ReviewOutcome {
            reviewer: ModelIdentity {
                provider: "openrouter".into(),
                model: reviewer_model.into(),
                runtime: "native".into(),
            },
            max_rounds: 3,
            rounds: vec![
                round(
                    1,
                    ReviewVerdictKind::Changes,
                    "- **F1**: src/cart.rs:3: off by one\n- **F2**: src/api.rs:9: unchecked input",
                    true,
                ),
                round(
                    2,
                    ReviewVerdictKind::Changes,
                    "F1: src/cart.rs:12: the total ignores the coupon",
                    true,
                ),
                round(3, ReviewVerdictKind::Approve, "Nothing must change.", false),
            ],
            passed: true,
            state: "approved".into(),
            reason: "The reviewer approved this result in round 3 of 3.".into(),
        });
        observed
    }

    #[tokio::test]
    async fn adjudications_are_recorded_for_every_finding_sent_back() {
        let run = fix_context(WRITER, OTHER);
        let third = adjudications(&[("F1", "accept", "applied the coupon before tax")]);
        let report = fix_report(&run, reviewed_turn("openai/gpt-oss-120b", Some(&third)));
        let seen: Vec<_> = report
            .adjudications
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
            [
                (1, "F1", AdjudicationDecision::Accept, Some(2)),
                (1, "F2", AdjudicationDecision::Reject, Some(2)),
                (2, "F1", AdjudicationDecision::Accept, Some(3)),
            ]
        );
        assert_eq!(
            report.adjudications[1].reason,
            "the input is validated by the caller in api.rs:40"
        );
        assert_eq!(report.adjudications[0].finding, "src/cart.rs:3: off by one");
        // Each round's findings are split by id; an approval adds none.
        let review = report.turns[0].review.as_ref().unwrap();
        let ids: Vec<Vec<&str>> = review
            .rounds
            .iter()
            .map(|r| r.findings.iter().map(|f| f.id.as_str()).collect())
            .collect();
        assert_eq!(ids, [vec!["F1", "F2"], vec!["F1"], vec![]]);
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
        assert!(!report.fail_on_findings);
        assert_eq!(report.turn_refs[0].purpose, "run");

        // The events: the review phase, then each adjudication.
        let host = FakeHost::default();
        record_report(&host, &run, &report).await.unwrap();
        let events = host.events();
        assert!(matches!(&events[0], RunEvent::Phase { phase, detail, .. }
            if phase == "review" && detail == "3 review rounds; 3 adjudications, 0 missing"));
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, RunEvent::Adjudication { .. }))
                .count(),
            3
        );
    }

    #[test]
    fn a_writer_that_does_not_answer_leaves_missing_adjudications() {
        let run = fix_context(WRITER, OTHER);
        // The third generation answered without a block.
        let report = fix_report(
            &run,
            reviewed_turn("openai/gpt-oss-120b", Some("I fixed it.")),
        );
        let last = report.adjudications.last().unwrap();
        assert_eq!(last.decision, AdjudicationDecision::Missing);
        assert_eq!(last.reason, axocoatl_session::review_adjudication::NO_BLOCK);
        // The writer never ran for round 2.
        let report = fix_report(&run, reviewed_turn("openai/gpt-oss-120b", None));
        let last = report.adjudications.last().unwrap();
        assert_eq!(last.decision, AdjudicationDecision::Missing);
        assert!(last.reason.contains("did not run again"));
        // Answers to ids the round lacks are noted as warnings.
        let third = adjudications(&[
            ("F1", "accept", "done"),
            ("F7", "reject", "no such finding"),
        ]);
        let report = fix_report(&run, reviewed_turn("openai/gpt-oss-120b", Some(&third)));
        assert_eq!(report.warnings.len(), 1);
        assert_eq!(report.warnings[0].code, IGNORED_ADJUDICATION);
        assert!(report.warnings[0].message.contains("F7"));
    }

    #[tokio::test]
    async fn a_reviewer_on_the_writers_model_is_warned_about_not_refused() {
        let run = fix_context(WRITER, WRITER);
        assert!(run
            .resolved
            .warnings
            .iter()
            .any(|warning| warning.code == SAME_MODEL_REVIEWER));
        let third = adjudications(&[("F1", "accept", "done")]);
        let report = fix_report(&run, reviewed_turn("qwen/qwen3-coder", Some(&third)));
        assert_eq!(report.warnings.len(), 1, "{:?}", report.warnings);
        assert_eq!(report.warnings[0].code, SAME_MODEL_REVIEWER);
        assert!(report.warnings[0]
            .message
            .contains("openrouter:qwen/qwen3-coder"));
        // The run still has its adjudications: a warning, not a refusal.
        assert_eq!(report.adjudications.len(), 3);
        // Resolution already recorded it when the run was accepted, so the
        // driver does not record it twice.
        let host = FakeHost::default();
        record_report(&host, &run, &report).await.unwrap();
        assert!(!host
            .events()
            .iter()
            .any(|event| matches!(event, RunEvent::Warning { .. })));

        // A reviewer that actually ran on the writer's model, though the
        // loadout resolved another, is warned about and recorded.
        let run = fix_context(WRITER, OTHER);
        let report = fix_report(&run, reviewed_turn("qwen/qwen3-coder", Some(&third)));
        assert_eq!(report.warnings.len(), 1);
        assert_eq!(report.warnings[0].code, SAME_MODEL_REVIEWER);
        let host = FakeHost::default();
        record_report(&host, &run, &report).await.unwrap();
        assert!(host.events().iter().any(|event| matches!(event,
            RunEvent::Warning { warning, .. } if warning.code == SAME_MODEL_REVIEWER)));
    }

    #[test]
    fn adjudicate_false_records_no_adjudications() {
        let mut run = fix_context(WRITER, OTHER);
        run.resolved
            .loadout
            .file
            .review
            .as_mut()
            .unwrap()
            .adjudicate = false;
        let report = fix_report(&run, reviewed_turn("openai/gpt-oss-120b", None));
        assert!(report.adjudications.is_empty());
        // Findings are still split for the Outcome.
        assert_eq!(
            report.turns[0].review.as_ref().unwrap().rounds[0]
                .findings
                .len(),
            2
        );
    }

    /// The driver runs the loadout's one turn (admission has filled the
    /// detected check) and then the report, recording its adjudications.
    #[tokio::test]
    async fn the_driver_reports_the_observed_turn() {
        let mut run = fix_context(WRITER, WRITER);
        crate::loadout::team_plan::fill_detected_checks(
            &mut run.resolved,
            Some("cargo test"),
            None,
        )
        .unwrap();
        let third = adjudications(&[("F1", "accept", "done")]);
        let observed = reviewed_turn("qwen/qwen3-coder", Some(&third));
        let host = FakeHost::with_turn(observed);
        let report = FixDriver.drive(&host, &run).await.unwrap();
        assert_eq!(report.adjudications.len(), 3);
        assert_eq!(report.warnings[0].code, SAME_MODEL_REVIEWER);
        assert_eq!(host.sent.lock().unwrap().len(), 1);
        assert!(host
            .events()
            .iter()
            .any(|event| matches!(event, RunEvent::Adjudication { .. })));
    }
}
