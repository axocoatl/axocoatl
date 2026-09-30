use super::*;
use axocoatl_core::TokenUsageStats;

// Numerical admission budgets are fixtures only, never production defaults.
const FIXTURE_LIMITS: WaysRetentionLimits = WaysRetentionLimits {
    version: 1,
    field_bytes: 512 * 1024,
    record_bytes: 4 * 1024 * 1024,
    aggregate_bytes: 64 * 1024 * 1024,
    records: 128,
    candidates: 100,
    items_per_field: 256,
};

fn evidence(value: &str) -> EvidenceRef {
    EvidenceRef::new(value).unwrap()
}
fn usage_fixture(id: &str) -> WaysUsage {
    WaysUsage {
        measurement_id: evidence(id),
        tokens: ExecutionUsage::Unknown {
            known_subtotal: TokenUsageStats::default(),
        },
        cost_usd_known_subtotal: 0.125,
        cost_complete: false,
    }
}
fn fixture() -> WaysDecisionRecord {
    let set_id = WaysSetId(evidence("set-a"));
    let id = WaysCandidateId {
        set_id: set_id.clone(),
        index: 0,
    };
    let patch = ProtectedWaysPatch {
        candidate: id.clone(),
        base_commit_oid: "1".repeat(40),
        base_tree_oid: "2".repeat(40),
        candidate_commit_oid: "3".repeat(40),
        candidate_tree_oid: "4".repeat(40),
        patch_sha256: sha256(b"exact protected binary patch"),
        patch_bytes: 28,
        protected_artifact_ref: evidence("protected-patch-a"),
    };
    let model = ExecutionModelRef {
        provider_id: "ollama".into(),
        model_id: "local-test".into(),
        configuration_ref: evidence("model-config-a"),
    };
    WaysDecisionRecord {
        schema_version: WAYS_DECISION_SCHEMA_VERSION,
        retention_limits_version: WAYS_RETENTION_LIMITS_VERSION,
        decision_id: DecisionId(evidence("decision-a")),
        session_id: SessionId::new("session-a").unwrap(),
        source_turn_id: LogicalTurnId::new("turn-request").unwrap(),
        set_id,
        task: ReviewText::complete("Repair the QA regression"),
        starting_repository: WaysRepositoryIdentity {
            workspace_id: evidence("workspace-a"),
            repository_ref: evidence("repository-owner-a"),
            commit_oid: "1".repeat(40),
            tree_oid: "2".repeat(40),
        },
        candidates: vec![WaysCandidateEvidence {
            id: id.clone(),
            agent: AgentDefinitionId::new("developer").unwrap(),
            model: Recorded::Available {
                value: model.clone(),
            },
            isolation: Recorded::Available {
                value: "podman".into(),
            },
            terminal: WaysTerminalState::Completed,
            failure_or_no_change_reason: Recorded::Unavailable {
                reason: UnavailableReason::NotProduced,
            },
            outcome: ReviewText::complete("Fixed the regression"),
            route: ReviewText::complete("Read, edit, check"),
            tools: RetainedItems {
                items: vec![WaysToolReference {
                    invocation_id: InvocationId::new("invocation-a").unwrap(),
                    event_ref: evidence("tool-event-a"),
                    detail: ReviewText::complete("edit_file: src/lib.rs"),
                }],
                original_count: 1,
            },
            changed_paths: RetainedItems {
                items: vec!["src/lib.rs".into()],
                original_count: 1,
            },
            patch: Recorded::Available {
                value: patch.clone(),
            },
            reviewable_diff: ReviewText::complete(
                "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n-old\n+new\n",
            ),
            checks: vec![WaysCheckEvidence {
                check_id: evidence("check-a"),
                command: Recorded::Available {
                    value: "cargo test".into(),
                },
                outcome: WaysCheckOutcome::Passed,
                exit_code: Recorded::Available { value: 0 },
                output: ReviewText::complete("1 passed"),
                duration_ms: Recorded::Available { value: 245 },
                checked_patch: Recorded::Available {
                    value: patch.clone(),
                },
            }],
            usage: usage_fixture("candidate-usage-a"),
        }],
        judge: Some(WaysJudgeEvidence {
            judgment_id: evidence("judge-a"),
            criteria: ReviewText::complete("Correctness first"),
            model,
            candidate_patches: vec![patch.clone()],
            result: ReviewText::complete("Candidate 0 fixes the regression"),
            recommended: Some(id.clone()),
            usage: usage_fixture("judge-usage-a"),
        }),
        shared_usage: vec![usage_fixture("probe-usage-a")],
        human_decision: WaysHumanDecision {
            decision_intent_id: evidence("human-intent-a"),
            decided_at_unix_ms: 1000,
            choice: WaysHumanChoice::Keep {
                patch: patch.clone(),
            },
        },
        application: WaysApplicationOutcome::Pending {
            identity: WaysApplicationIdentity {
                operation_id: evidence("existing-keep-operation-a"),
                patch,
                preimage_tree_oid: "5".repeat(40),
                postimage_tree_oid: "6".repeat(40),
            },
        },
        selected_session_turn: None,
        cleanup: WaysCleanupEvidence {
            inventory_complete: true,
            targets: vec![WaysCleanupTarget {
                candidate: id,
                kind: WaysResourceKind::Sandbox,
                backend: "podman".into(),
                resource_id: "exact-container-a".into(),
                ownership_ref: evidence("container-owner-a"),
                outcome: WaysCleanupOutcome::Pending,
            }],
            completed_at_unix_ms: None,
        },
    }
}

#[test]
fn retained_decision_round_trip_keeps_bytes_and_unknown_accounting() {
    let record = fixture();
    let bytes = serde_json::to_vec(&record).unwrap();
    let decoded = WaysDecisionRecord::from_json(&bytes, FIXTURE_LIMITS).unwrap();
    assert_eq!(decoded, record);
    assert!(matches!(
        decoded.candidates[0].usage.tokens,
        ExecutionUsage::Unknown { .. }
    ));
    assert_eq!(decoded.candidates[0].usage.cost_usd_known_subtotal, 0.125);
    assert!(!decoded.candidates[0].usage.cost_complete);
    assert!(matches!(
        decoded.application,
        WaysApplicationOutcome::Pending { .. }
    ));
    assert!(decoded.selected_session_turn.is_none());
}

#[test]
fn serialized_failed_check_fixture_preserves_schema_and_absent_effects() {
    let bytes = include_bytes!("../tests/fixtures/ways-decision/failed-check-no-keep-v1.json");
    let record = WaysDecisionRecord::from_json(bytes, FIXTURE_LIMITS).unwrap();
    assert_eq!(
        serde_json::to_value(&record).unwrap(),
        serde_json::from_slice::<serde_json::Value>(bytes).unwrap(),
        "schema changes must not silently rewrite retained decision evidence"
    );
    assert!(matches!(
        record.human_decision.choice,
        WaysHumanChoice::NoKeep
    ));
    assert!(record.selected_session_turn.is_none());
    let check = &record.candidates[0].checks[0];
    assert!(matches!(
        check.outcome,
        WaysCheckOutcome::VerificationRejected { .. }
    ));
    assert!(matches!(
        check.exit_code,
        Recorded::Unavailable {
            reason: UnavailableReason::NotProduced
        }
    ));
    assert!(matches!(
        check.checked_patch,
        Recorded::Unavailable {
            reason: UnavailableReason::NotProduced
        }
    ));
    assert!(!record.cleanup.inventory_complete);
    assert!(record.cleanup.completed_at_unix_ms.is_none());
}

#[test]
fn truncated_tail_and_unavailable_evidence_are_explicit_and_reviewable() {
    let mut record = fixture();
    let source = "full check log: passed";
    record.candidates[0].checks[0].output = ReviewText::Truncated {
        text: "passed".into(),
        retained_sha256: sha256(b"passed"),
        original_sha256: sha256(source.as_bytes()),
        original_bytes: source.len() as u64,
        offset_bytes: (source.len() - 6) as u64,
    };
    record.candidates[0].route = ReviewText::Unavailable {
        reason: UnavailableReason::NotRecorded,
        detail: "Original Route events were not recorded".into(),
    };
    record.candidates[0].changed_paths.original_count = 3;
    record.validate(FIXTURE_LIMITS).unwrap();
    assert!(record.candidates[0].changed_paths.is_truncated());
    if let ReviewText::Truncated { text, .. } = &mut record.candidates[0].checks[0].output {
        text.clear();
    }
    assert!(
        record.validate(FIXTURE_LIMITS).is_err(),
        "hash-only truncation is not reviewable evidence"
    );
}

#[test]
fn every_retention_boundary_rejects_without_eviction_or_mutation() {
    let record = fixture();
    let limits = FIXTURE_LIMITS;
    let size = record.validate(limits).unwrap();
    let exact = WaysRetentionLimits {
        field_bytes: 1024,
        record_bytes: size,
        aggregate_bytes: size,
        records: 1,
        ..limits
    };
    assert_eq!(
        validate_ways_retention(std::slice::from_ref(&record), exact).unwrap(),
        size
    );
    let before = serde_json::to_vec(&record).unwrap();
    assert!(matches!(
        record.validate(WaysRetentionLimits {
            record_bytes: size - 1,
            ..exact
        }),
        Err(WaysDecisionError::Capacity)
    ));
    let mut large_field = record.clone();
    large_field.task = ReviewText::complete("x".repeat(1025));
    assert!(matches!(
        large_field.validate(exact),
        Err(WaysDecisionError::Capacity)
    ));
    let mut second = record.clone();
    second.decision_id = DecisionId(evidence("decision-b"));
    second.session_id = SessionId::new("session-b").unwrap();
    let second_size = second.validate(limits).unwrap();
    let aggregate = WaysRetentionLimits {
        field_bytes: 1024,
        record_bytes: size.max(second_size),
        aggregate_bytes: size + second_size - 1,
        records: 2,
        ..limits
    };
    assert!(matches!(
        validate_ways_retention(&[record.clone(), second], aggregate),
        Err(WaysDecisionError::Capacity)
    ));
    assert_eq!(serde_json::to_vec(&record).unwrap(), before);
    assert!(matches!(
        validate_ways_retention(&[record.clone(), record.clone()], exact),
        Err(WaysDecisionError::Capacity)
    ));
    let mut version = limits;
    version.version = 2;
    assert!(matches!(
        record.validate(version),
        Err(WaysDecisionError::Version)
    ));
}

#[test]
fn stale_candidate_check_judge_and_selected_patch_are_rejected() {
    for mutate in 0..4 {
        let mut record = fixture();
        match mutate {
            0 => {
                let Recorded::Available { value } =
                    &mut record.candidates[0].checks[0].checked_patch
                else {
                    panic!("fixture")
                };
                value.patch_sha256 = "f".repeat(64);
            }
            1 => {
                record.judge.as_mut().unwrap().candidate_patches[0].candidate_tree_oid =
                    "f".repeat(40)
            }
            2 => {
                if let WaysHumanChoice::Keep { patch } = &mut record.human_decision.choice {
                    patch.candidate.index = 1;
                }
            }
            _ => {
                if let WaysApplicationOutcome::Pending { identity } = &mut record.application {
                    identity.patch.patch_sha256 = "f".repeat(64);
                }
            }
        }
        assert!(
            record.validate(FIXTURE_LIMITS).is_err(),
            "mutation {mutate}"
        );
    }
}

#[test]
fn capture_failure_retains_failed_check_without_inventing_a_patch() {
    let mut record = fixture();
    record.judge = None;
    record.human_decision.choice = WaysHumanChoice::NoKeep;
    record.application = WaysApplicationOutcome::NoKeepRecorded {
        receipt_ref: evidence("capture-failure-no-keep"),
        recorded_at_unix_ms: 2000,
    };
    let candidate = &mut record.candidates[0];
    candidate.patch = Recorded::Unavailable {
        reason: UnavailableReason::NotProduced,
    };
    candidate.reviewable_diff = ReviewText::Unavailable {
        reason: UnavailableReason::NotProduced,
        detail: "Candidate capture failed before a protected patch was produced".into(),
    };
    let check = &mut candidate.checks[0];
    check.outcome = WaysCheckOutcome::Failed;
    // Existing capture failures retain -1 in LaneVerdict. Preserve that
    // evidence here; this fixture does not claim a process actually exited -1.
    check.exit_code = Recorded::Available { value: -1 };
    check.output = ReviewText::complete("capturing candidate tree: fixture capture failure");
    check.checked_patch = Recorded::Unavailable {
        reason: UnavailableReason::NotProduced,
    };
    let bytes = serde_json::to_vec(&record).unwrap();
    assert_eq!(
        WaysDecisionRecord::from_json(&bytes, FIXTURE_LIMITS).unwrap(),
        record
    );

    // A known host failure also remains representable when no process exit
    // was observed; do not manufacture the legacy sentinel for new evidence.
    record.candidates[0].checks[0].exit_code = Recorded::Unavailable {
        reason: UnavailableReason::NotProduced,
    };
    record.candidates[0].checks[0].outcome = WaysCheckOutcome::VerificationRejected {
        reason: ReviewText::complete("Check dispatch failed before a process was started"),
    };
    record.candidates[0].checks[0].output = ReviewText::Unavailable {
        reason: UnavailableReason::NotProduced,
        detail: "No check process was started, so it produced no output".into(),
    };
    let bytes = serde_json::to_vec(&record).unwrap();
    assert_eq!(
        WaysDecisionRecord::from_json(&bytes, FIXTURE_LIMITS).unwrap(),
        record
    );

    record.candidates[0].checks[0].outcome = WaysCheckOutcome::Passed;
    record.candidates[0].checks[0].exit_code = Recorded::Available { value: 0 };
    assert!(
        record.validate(FIXTURE_LIMITS).is_err(),
        "an unavailable checked patch cannot become passing verification"
    );
}

#[test]
fn host_gitlink_rejection_preserves_successful_command_exit_and_reason() {
    let mut record = fixture();
    record.judge = None;
    record.human_decision.choice = WaysHumanChoice::NoKeep;
    record.application = WaysApplicationOutcome::NoKeepRecorded {
        receipt_ref: evidence("gitlink-no-keep"),
        recorded_at_unix_ms: 2000,
    };
    let reason = "Axocoatl cannot safely Keep submodule/gitlink changes yet.";
    let check = &mut record.candidates[0].checks[0];
    check.exit_code = Recorded::Available { value: 0 };
    check.output = ReviewText::complete(format!("1 passed\n{reason}"));
    check.outcome = WaysCheckOutcome::VerificationRejected {
        reason: ReviewText::complete(reason),
    };
    let bytes = serde_json::to_vec(&record).unwrap();
    assert_eq!(
        WaysDecisionRecord::from_json(&bytes, FIXTURE_LIMITS).unwrap(),
        record
    );
    assert_eq!(
        record.candidates[0].checks[0].exit_code,
        Recorded::Available { value: 0 }
    );

    record.candidates[0].checks[0].outcome = WaysCheckOutcome::VerificationRejected {
        reason: ReviewText::Unavailable {
            reason: UnavailableReason::NotRecorded,
            detail: "The original host rejection reason was not recorded".into(),
        },
    };
    record.validate(FIXTURE_LIMITS).unwrap();
    record.candidates[0].checks[0].outcome = WaysCheckOutcome::VerificationRejected {
        reason: ReviewText::complete("x".repeat(FIXTURE_LIMITS.field_bytes + 1)),
    };
    assert!(matches!(
        record.validate(FIXTURE_LIMITS),
        Err(WaysDecisionError::Capacity)
    ));
}

#[test]
fn cleanup_is_separate_from_keep_intent_application_and_session_link() {
    let mut record = fixture();
    record.cleanup.targets[0].outcome = WaysCleanupOutcome::Completed {
        receipt_ref: evidence("cleanup-a"),
        completed_at_unix_ms: 3000,
    };
    record.cleanup.completed_at_unix_ms = Some(3000);
    assert!(record.validate(FIXTURE_LIMITS).is_err());
    let WaysApplicationOutcome::Pending { identity } = record.application.clone() else {
        panic!("fixture")
    };
    record.application = WaysApplicationOutcome::Applied {
        identity,
        receipt_ref: evidence("keep-receipt-a"),
        applied_at_unix_ms: 2000,
    };
    assert!(
        record.validate(FIXTURE_LIMITS).is_err(),
        "application alone does not prove Session transcript commit"
    );
    record.selected_session_turn = Some(WaysSelectedSessionTurn {
        session_id: record.session_id.clone(),
        turn_id: LogicalTurnId::new("kept-turn-a").unwrap(),
        transcript_receipt_ref: evidence("transcript-a"),
    });
    record.validate(FIXTURE_LIMITS).unwrap();
    record.cleanup.inventory_complete = false;
    assert!(record.validate(FIXTURE_LIMITS).is_err());
}

#[test]
fn no_keep_result_does_not_claim_application_or_session_link() {
    let mut record = fixture();
    record.human_decision.choice = WaysHumanChoice::NoKeep;
    assert!(
        record.validate(FIXTURE_LIMITS).is_err(),
        "pending Keep contradicts no-Keep"
    );
    record.application = WaysApplicationOutcome::NoKeepRecorded {
        receipt_ref: evidence("no-keep-a"),
        recorded_at_unix_ms: 2000,
    };
    record.validate(FIXTURE_LIMITS).unwrap();
    record.selected_session_turn = Some(WaysSelectedSessionTurn {
        session_id: record.session_id.clone(),
        turn_id: LogicalTurnId::new("invented-kept-turn").unwrap(),
        transcript_receipt_ref: evidence("invented-transcript"),
    });
    assert!(record.validate(FIXTURE_LIMITS).is_err());
}

#[test]
fn unknown_schema_duplicate_usage_and_digest_tampering_fail_closed() {
    let record = fixture();
    let mut value = serde_json::to_value(&record).unwrap();
    value["schema_version"] = 2.into();
    assert!(matches!(
        WaysDecisionRecord::from_json(&serde_json::to_vec(&value).unwrap(), FIXTURE_LIMITS),
        Err(WaysDecisionError::Version)
    ));
    let mut changed = record.clone();
    changed.shared_usage[0].measurement_id = changed.candidates[0].usage.measurement_id.clone();
    assert!(changed.validate(FIXTURE_LIMITS).is_err());
    changed = record;
    if let ReviewText::Complete { text, .. } = &mut changed.candidates[0].reviewable_diff {
        text.push_str("tampered");
    }
    assert!(changed.validate(FIXTURE_LIMITS).is_err());
}

#[test]
fn cleanup_cannot_delete_its_retained_evidence_or_starting_repository_promise() {
    for starting_repository in [false, true] {
        let mut record = fixture();
        record.cleanup.targets[0].kind = WaysResourceKind::DisposableCandidateGitRefs;
        record.cleanup.targets[0].ownership_ref = if starting_repository {
            record.starting_repository.repository_ref.clone()
        } else {
            let Recorded::Available { value } = &record.candidates[0].patch else {
                panic!("fixture")
            };
            value.protected_artifact_ref.clone()
        };
        assert!(matches!(
            record.validate(FIXTURE_LIMITS),
            Err(WaysDecisionError::Invalid(
                "cleanup targets retained evidence or starting repository ownership"
            ))
        ));
    }
    // Separate names are necessary but do not authenticate resource ownership
    // or prove that the candidate refs and retained Git objects are disjoint.
    let mut separate = fixture();
    separate.cleanup.targets[0].kind = WaysResourceKind::DisposableCandidateGitRefs;
    separate.validate(FIXTURE_LIMITS).unwrap();
}

#[test]
fn callers_supply_budgets_without_inheriting_test_fixture_ceilings() {
    let larger = WaysRetentionLimits {
        field_bytes: 1024 * 1024,
        record_bytes: 8 * 1024 * 1024,
        aggregate_bytes: 128 * 1024 * 1024,
        records: 256,
        items_per_field: 1024,
        ..FIXTURE_LIMITS
    };
    fixture().validate(larger).unwrap();
    let unsupported_object = WaysRetentionLimits {
        record_bytes: MAX_SERIALIZED_DECISION_BYTES + 1,
        aggregate_bytes: MAX_SERIALIZED_DECISION_BYTES + 1,
        ..larger
    };
    assert!(matches!(
        fixture().validate(unsupported_object),
        Err(WaysDecisionError::Capacity)
    ));
}
