use super::*;
use crate::execution_ownership::LegacyFormatOwnership;
use std::sync::Arc;

const LIMITS: WaysRetentionLimits = WaysRetentionLimits {
    version: 1,
    field_bytes: 16 * 1024,
    record_bytes: 32 * 1024,
    aggregate_bytes: 128 * 1024,
    records: 4,
    candidates: 4,
    items_per_field: 16,
};
fn evidence(value: &str) -> EvidenceRef {
    EvidenceRef::new(value).unwrap()
}
fn fixture() -> (
    tempfile::TempDir,
    SessionExecutionStore,
    WaysDecisionRecord,
    PinnedWaysPatch,
) {
    let root = tempfile::tempdir().unwrap();
    let ownership = Arc::new(
        LegacyFormatOwnership::acquire(root.path())
            .unwrap()
            .upgrade()
            .unwrap(),
    );
    let mut record = WaysDecisionRecord::from_json(
        include_bytes!("../tests/fixtures/ways-decision/failed-check-no-keep-v1.json"),
        LIMITS,
    )
    .unwrap();
    let canonical = SessionExecutionStore::open(
        ownership,
        ExecutionStoreOwner {
            workspace_id: "workspace-client".into(),
            session_id: record.session_id.clone(),
        },
    )
    .unwrap();
    let patch = PinnedWaysPatch::capture(b"--- a/file\n+++ b/file\n@@ -1 +1 @@\n-before\n+after\n")
        .unwrap();
    record.candidates[0].patch = Recorded::Available {
        value: ProtectedWaysPatch {
            candidate: record.candidates[0].id.clone(),
            base_commit_oid: record.starting_repository.commit_oid.clone(),
            base_tree_oid: record.starting_repository.tree_oid.clone(),
            candidate_commit_oid: "3".repeat(40),
            candidate_tree_oid: "4".repeat(40),
            patch_sha256: patch.sha256.clone(),
            patch_bytes: patch.byte_len,
            protected_artifact_ref: patch.reference.clone(),
        },
    };
    record.reviewable_diff_for_fixture(&patch);
    record.application = WaysApplicationOutcome::NotStarted;
    record.cleanup.inventory_complete = true;
    (root, canonical, record, patch)
}
impl WaysDecisionRecord {
    fn reviewable_diff_for_fixture(&mut self, patch: &PinnedWaysPatch) {
        self.candidates[0].reviewable_diff =
            ReviewText::complete(String::from_utf8(patch.bytes().unwrap()).unwrap());
    }
}
fn open(canonical: &SessionExecutionStore) -> WaysDecisionStore {
    WaysDecisionStore::open_owned(
        canonical
            .component_namespace(ExecutionComponent::WaysDecisions)
            .unwrap(),
        canonical,
        LIMITS,
    )
    .unwrap()
}

#[test]
fn archive_freezes_reviewable_evidence_and_patch_across_restart() {
    let (_root, canonical, record, patch) = fixture();
    let mut store = open(&canonical);
    store.freeze(record.clone(), vec![patch.clone()]).unwrap();
    drop(store);
    let store = open(&canonical);
    assert_eq!(store.get(&record.decision_id).unwrap(), Some(&record));
    assert_eq!(
        store.patch(&patch.reference).unwrap().unwrap(),
        patch.bytes().unwrap()
    );
    assert!(matches!(
        store.records().unwrap()[0].candidates[0].usage.tokens,
        crate::execution_content::ExecutionUsage::Unknown { .. }
    ));
}

#[test]
fn missing_or_substituted_patch_cannot_authorize_cleanup_or_replace_evidence() {
    let (_root, canonical, record, patch) = fixture();
    let mut store = open(&canonical);
    assert!(store.freeze(record.clone(), vec![]).is_err());
    assert!(store.records().unwrap().is_empty());
    let mut wrong = patch.clone();
    wrong.base64 = base64::engine::general_purpose::STANDARD.encode(b"changed");
    assert!(store.freeze(record.clone(), vec![wrong]).is_err());
    assert!(store.records().unwrap().is_empty());
    store.freeze(record.clone(), vec![patch.clone()]).unwrap();
    let mut changed = record.clone();
    changed.task = ReviewText::complete("Different intent");
    assert!(store.freeze(changed, vec![patch]).is_err());
    assert_eq!(store.get(&record.decision_id).unwrap(), Some(&record));
}

#[test]
fn no_keep_settlement_precedes_cleanup_and_deleted_reference_stays_unavailable() {
    let (_root, canonical, mut record, patch) = fixture();
    let mut store = open(&canonical);
    store.freeze(record.clone(), vec![patch.clone()]).unwrap();
    record.cleanup.completed_at_unix_ms = Some(3);
    assert!(store.record_progress(record.clone()).is_err());
    record.cleanup.completed_at_unix_ms = None;
    record.application = WaysApplicationOutcome::NoKeepRecorded {
        receipt_ref: evidence("no-keep-receipt"),
        recorded_at_unix_ms: 2,
    };
    store.record_progress(record.clone()).unwrap();
    record.cleanup.completed_at_unix_ms = Some(3);
    store.record_progress(record.clone()).unwrap();
    assert!(store
        .delete_verified(&record.decision_id, 4, |_| Err(WaysArchiveError::Invalid(
            "external reference ownership unresolved"
        )))
        .is_err());
    assert!(store.patch(&patch.reference).unwrap().is_some());
    store
        .delete_verified(&record.decision_id, 4, |_| {
            Ok(evidence("verified-delete-receipt"))
        })
        .unwrap();
    drop(store);
    let mut store = open(&canonical);
    assert!(store.get(&record.decision_id).unwrap().is_none());
    assert!(store.patch(&patch.reference).unwrap().is_none());
    assert_eq!(store.deleted().unwrap()[0].decision_id, record.decision_id);
    assert!(store.freeze(record, vec![patch]).is_err());
}

#[test]
fn keep_cannot_skip_pending_identity_or_change_selected_patch_after_freeze() {
    let (_root, canonical, mut record, patch) = fixture();
    let mut store = open(&canonical);
    let Recorded::Available { value: protected } = record.candidates[0].patch.clone() else {
        unreachable!()
    };
    record.human_decision.choice = WaysHumanChoice::Keep {
        patch: protected.clone(),
    };
    store.freeze(record.clone(), vec![patch]).unwrap();
    let identity = WaysApplicationIdentity {
        operation_id: evidence("existing-keep-transaction"),
        patch: protected,
        preimage_tree_oid: "5".repeat(40),
        postimage_tree_oid: "6".repeat(40),
    };
    record.application = WaysApplicationOutcome::Applied {
        identity: identity.clone(),
        receipt_ref: evidence("actual-application-receipt"),
        applied_at_unix_ms: 3,
    };
    assert!(store.record_progress(record.clone()).is_err());
    record.application = WaysApplicationOutcome::Pending {
        identity: identity.clone(),
    };
    store.record_progress(record.clone()).unwrap();
    let mut wrong = record.clone();
    if let WaysApplicationOutcome::Pending { identity } = &mut wrong.application {
        identity.postimage_tree_oid = "7".repeat(40);
    }
    assert!(store.record_progress(wrong).is_err());
    record.application = WaysApplicationOutcome::Applied {
        identity,
        receipt_ref: evidence("actual-application-receipt"),
        applied_at_unix_ms: 3,
    };
    store.record_progress(record.clone()).unwrap();
    record.cleanup.completed_at_unix_ms = Some(4);
    assert!(store.record_progress(record.clone()).is_err());
    record.selected_session_turn = Some(WaysSelectedSessionTurn {
        session_id: record.session_id.clone(),
        turn_id: crate::turn_contract::LogicalTurnId::new("kept-turn").unwrap(),
        transcript_receipt_ref: evidence("actual-session-link"),
    });
    store.record_progress(record).unwrap();
}

#[test]
fn lost_initialized_archive_is_not_recreated_as_empty() {
    let (_root, canonical, record, patch) = fixture();
    let mut store = open(&canonical);
    store.freeze(record, vec![patch]).unwrap();
    drop(store);
    let file = canonical
        .path()
        .parent()
        .unwrap()
        .join("ways-decisions")
        .join(WAYS_ARCHIVE_FILE);
    std::fs::remove_file(file).unwrap();
    let namespace = canonical
        .component_namespace(ExecutionComponent::WaysDecisions)
        .unwrap();
    assert!(WaysDecisionStore::open_owned(namespace, &canonical, LIMITS).is_err());
}

#[test]
fn aggregate_capacity_includes_pin_bytes_and_never_evicts_existing_record() {
    let (_root, canonical, record, patch) = fixture();
    let mut store = open(&canonical);
    store.freeze(record.clone(), vec![patch]).unwrap();
    let mut overflow = record.clone();
    overflow.decision_id = DecisionId(evidence("another-decision"));
    overflow.set_id = WaysSetId(evidence("another-set"));
    overflow.candidates[0].id.set_id = overflow.set_id.clone();
    let large = PinnedWaysPatch::capture(&vec![42; 100 * 1024]).unwrap();
    if let Recorded::Available { value } = &mut overflow.candidates[0].patch {
        value.candidate.set_id = overflow.set_id.clone();
        value.protected_artifact_ref = large.reference.clone();
        value.patch_sha256 = large.sha256.clone();
        value.patch_bytes = large.byte_len;
    }
    assert!(store.freeze(overflow, vec![large]).is_err());
    assert_eq!(store.records().unwrap(), &[record]);
}

#[test]
fn unfinished_decision_reserves_final_receipts_and_limits_can_expand_without_eviction() {
    let (_root, canonical, record, patch) = fixture();
    let mut limits = LIMITS;
    limits.aggregate_bytes = limits.record_bytes;
    let namespace = canonical
        .component_namespace(ExecutionComponent::WaysDecisions)
        .unwrap();
    let mut store = WaysDecisionStore::open_owned(namespace, &canonical, limits).unwrap();
    // Its current short record would fit, but the complete finalization envelope
    // plus archive framing and protected bytes would not.
    assert!(store.freeze(record.clone(), vec![patch.clone()]).is_err());
    assert!(store.records().unwrap().is_empty());
    store.configure_limits(LIMITS).unwrap();
    store.freeze(record.clone(), vec![patch.clone()]).unwrap();
    assert!(store.configure_limits(limits).is_err());
    assert_eq!(store.records().unwrap(), std::slice::from_ref(&record));
    let changed = PinnedWaysPatch::capture(b"different retry body").unwrap();
    assert!(store.freeze(record, vec![changed]).is_err());
    assert_eq!(
        store.patch(&patch.reference).unwrap().unwrap(),
        patch.bytes().unwrap()
    );
}

#[test]
fn frozen_decision_must_fit_its_future_successful_completion_receipts() {
    let (_root, canonical, record, patch) = fixture();
    let initial = serde_json::to_vec(&record).unwrap().len();
    assert!(require_completion_capacity(&record, initial).is_err());
    let limits = WaysRetentionLimits {
        field_bytes: 1024,
        record_bytes: initial,
        ..LIMITS
    };
    let mut store = WaysDecisionStore::open_owned(
        canonical
            .component_namespace(ExecutionComponent::WaysDecisions)
            .unwrap(),
        &canonical,
        limits,
    )
    .unwrap();
    assert!(store.freeze(record, vec![patch]).is_err());
    assert!(store.records().unwrap().is_empty());
}
