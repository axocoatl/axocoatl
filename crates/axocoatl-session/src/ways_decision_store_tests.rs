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
    assert_eq!(
        store.get(&record.decision_id).unwrap(),
        Some(record.clone())
    );
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
    assert_eq!(
        store.get(&record.decision_id).unwrap(),
        Some(record.clone())
    );
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

/// Production sizes, but a segment holds three index entries, so a short test
/// spans many sealed segments.
const SMALL: SegmentSpec = SegmentSpec {
    segment_records: 3,
    segment_bytes: 64 * 1024,
    ..SPEC
};

fn namespace(canonical: &SessionExecutionStore) -> OwnedExecutionNamespace {
    canonical
        .component_namespace(ExecutionComponent::WaysDecisions)
        .unwrap()
}
fn open_small(canonical: &SessionExecutionStore) -> WaysDecisionStore {
    open_small_with(canonical, LIMITS)
}
fn open_small_with(
    canonical: &SessionExecutionStore,
    limits: WaysRetentionLimits,
) -> WaysDecisionStore {
    WaysDecisionStore::open_owned_with(namespace(canonical), canonical, limits, SMALL).unwrap()
}
fn component(canonical: &SessionExecutionStore) -> std::path::PathBuf {
    canonical.path().parent().unwrap().join("ways-decisions")
}
fn evidence_files(canonical: &SessionExecutionStore) -> std::collections::BTreeSet<String> {
    std::fs::read_dir(component(canonical).join(EVIDENCE_DIR))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect()
}
/// Another decision of the same Session, pinning its own patch.
fn decision(
    base: &WaysDecisionRecord,
    n: usize,
    patch_bytes: &[u8],
) -> (WaysDecisionRecord, PinnedWaysPatch) {
    let patch = PinnedWaysPatch::capture(patch_bytes).unwrap();
    let mut record = base.clone();
    record.decision_id = DecisionId(evidence(&format!("decision-{n}")));
    record.set_id = WaysSetId(evidence(&format!("ways-{n}")));
    record.candidates[0].id.set_id = record.set_id.clone();
    if let Recorded::Available { value } = &mut record.candidates[0].patch {
        value.candidate.set_id = record.set_id.clone();
        value.protected_artifact_ref = patch.reference.clone();
        value.patch_sha256 = patch.sha256.clone();
        value.patch_bytes = patch.byte_len;
    }
    record.candidates[0].reviewable_diff = ReviewText::complete(format!("patch {n}"));
    (record, patch)
}
fn settle(store: &mut WaysDecisionStore, record: &mut WaysDecisionRecord, at: u64) {
    record.application = WaysApplicationOutcome::NoKeepRecorded {
        receipt_ref: evidence("no-keep-receipt"),
        recorded_at_unix_ms: at,
    };
    store.record_progress(record.clone()).unwrap();
    record.cleanup.completed_at_unix_ms = Some(at + 1);
    store.record_progress(record.clone()).unwrap();
}
fn delete(store: &mut WaysDecisionStore, record: &WaysDecisionRecord, at: u64) {
    store
        .delete_verified(&record.decision_id, at, |_| {
            Ok(evidence("verified-delete-receipt"))
        })
        .unwrap();
}
fn capacity<T>(result: Result<T>) -> bool {
    matches!(
        result,
        Err(WaysArchiveError::Contract(WaysDecisionError::Capacity))
    )
}

#[test]
fn deleting_frees_retention_room_and_history_spans_sealed_segments() {
    let (_root, canonical, base, _) = fixture();
    // Room for four retained decisions, each holding its whole envelope.
    let limits = WaysRetentionLimits {
        aggregate_bytes: 4 * LIMITS.record_bytes + 1024,
        ..LIMITS
    };
    let mut store = open_small_with(&canonical, limits);
    // Ten decisions over time with room for four retained at once: each
    // deletion frees its room, and its tombstone is not counted.
    let mut deleted = vec![];
    for n in 0..10 {
        let (mut record, patch) = decision(&base, n, format!("patch body {n}\n").as_bytes());
        store.freeze(record.clone(), vec![patch.clone()]).unwrap();
        settle(&mut store, &mut record, 10 * n as u64);
        delete(&mut store, &record, 10 * n as u64 + 5);
        assert!(store.patch(&patch.reference).unwrap().is_none());
        deleted.push(record.decision_id);
    }
    assert!(
        store.log.sealed().len() >= 10,
        "history spans many segments"
    );
    let mut live = vec![];
    for n in 10..14 {
        let (record, patch) = decision(&base, n, format!("patch body {n}\n").as_bytes());
        store.freeze(record.clone(), vec![patch]).unwrap();
        live.push(record);
    }
    let (full, patch) = decision(&base, 14, b"patch body 14\n");
    assert!(capacity(store.freeze(full, vec![patch])));
    drop(store);

    let mut store = open_small_with(&canonical, limits);
    assert_eq!(store.records().unwrap(), live);
    assert_eq!(
        store
            .deleted()
            .unwrap()
            .into_iter()
            .map(|tombstone| tombstone.decision_id)
            .collect::<Vec<_>>(),
        deleted
    );
    // A tombstone in the oldest sealed segment still refuses re-creation, by
    // decision identity or by set.
    let (again, patch) = decision(&base, 0, b"patch body 0\n");
    assert!(matches!(
        store.freeze(again.clone(), vec![patch.clone()]),
        Err(WaysArchiveError::Invalid(
            "a deleted decision cannot be recreated"
        ))
    ));
    let mut same_set = again;
    same_set.decision_id = DecisionId(evidence("decision-new"));
    assert!(store.freeze(same_set, vec![patch]).is_err());
    assert_eq!(
        store
            .deleted_decision(&deleted[0])
            .unwrap()
            .unwrap()
            .decision_id,
        deleted[0]
    );
    assert!(store
        .deleted_decision(&live[0].decision_id)
        .unwrap()
        .is_none());
    // Only the retained decisions' records and patches remain on disk.
    assert_eq!(evidence_files(&canonical).len(), 2 * live.len());
}

#[test]
fn deleting_a_decision_frees_its_patch_bytes_for_another() {
    let (_root, canonical, base, _) = fixture();
    let mut store = open(&canonical);
    let (mut first, first_patch) = decision(&base, 1, &[1; 60 * 1024]);
    let (second, second_patch) = decision(&base, 2, &[2; 60 * 1024]);
    store
        .freeze(first.clone(), vec![first_patch.clone()])
        .unwrap();
    // Each unfinished decision holds its whole record envelope plus its patch.
    assert!(capacity(
        store.freeze(second.clone(), vec![second_patch.clone()])
    ));
    settle(&mut store, &mut first, 1);
    assert!(capacity(
        store.freeze(second.clone(), vec![second_patch.clone()])
    ));
    delete(&mut store, &first, 3);
    assert!(!evidence_files(&canonical).contains(&patch_file(&first_patch.sha256)));
    store.freeze(second.clone(), vec![second_patch]).unwrap();
    assert_eq!(store.records().unwrap(), vec![second]);
}

#[test]
fn one_patch_is_bounded_by_the_per_patch_limit() {
    assert!(capacity(PinnedWaysPatch::capture(&vec![
        0;
        MAX_WAYS_PATCH_BYTES
            + 1
    ])));
}

#[test]
fn a_torn_index_line_is_dropped_and_writing_continues() {
    let (_root, canonical, base, _) = fixture();
    let mut store = open(&canonical);
    let (first, first_patch) = decision(&base, 1, b"first\n");
    store.freeze(first.clone(), vec![first_patch]).unwrap();
    drop(store);
    let active = component(&canonical).join(SPEC.active_name());
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&active)
        .unwrap();
    std::io::Write::write_all(&mut file, br#"{"record":{"decision":{"decision_id""#).unwrap();
    drop(file);

    let mut store = open(&canonical);
    assert!(store.log.recovery().torn_bytes > 0);
    assert_eq!(store.records().unwrap(), vec![first.clone()]);
    let (second, second_patch) = decision(&base, 2, b"second\n");
    store.freeze(second.clone(), vec![second_patch]).unwrap();
    drop(store);
    assert_eq!(open(&canonical).records().unwrap(), vec![first, second]);
}

#[test]
fn an_interrupted_seal_is_completed_through_the_store() {
    let (_root, canonical, base, _) = fixture();
    let mut store = open_small(&canonical);
    let (mut record, patch) = decision(&base, 1, b"sealed\n");
    store.freeze(record.clone(), vec![patch]).unwrap();
    settle(&mut store, &mut record, 1);
    assert_eq!(store.log.sealed().len(), 1);
    drop(store);
    // The crash came after the sealed segment was published and before the
    // new active segment replaced the old one.
    let dir = component(&canonical);
    let sealed =
        std::fs::read(dir.join("segments").join("ways-decisions.0000000000.jsonl")).unwrap();
    let body_end = sealed[..sealed.len() - 1]
        .iter()
        .rposition(|byte| *byte == b'\n')
        .unwrap()
        + 1;
    std::fs::write(dir.join(SMALL.active_name()), &sealed[..body_end]).unwrap();

    let mut store = open_small(&canonical);
    assert!(store.log.recovery().completed_seal);
    assert_eq!(store.records().unwrap(), vec![record.clone()]);
    delete(&mut store, &record, 9);
    drop(store);
    let store = open_small(&canonical);
    assert!(store.records().unwrap().is_empty());
    assert_eq!(store.deleted().unwrap().len(), 1);
}

/// The single-file layout exactly as the earlier store wrote it.
#[derive(Serialize)]
struct SingleFileArchive<'a> {
    schema_version: u32,
    journal_id: &'a str,
    owner: &'a ExecutionStoreOwner,
    limits: WaysRetentionLimits,
    records: Vec<WaysDecisionRecord>,
    patches: Vec<PinnedWaysPatch>,
    deleted: Vec<DeletedWaysDecision>,
}

#[test]
fn a_single_file_store_is_converted_with_all_history_and_conversion_is_redone() {
    let (_root, canonical, base, _) = fixture();
    let (mut finished, finished_patch) = decision(&base, 1, b"finished\n");
    finished.application = WaysApplicationOutcome::NoKeepRecorded {
        receipt_ref: evidence("no-keep-receipt"),
        recorded_at_unix_ms: 2,
    };
    finished.cleanup.completed_at_unix_ms = Some(3);
    let (unfinished, unfinished_patch) = decision(&base, 2, b"unfinished\n");
    let tombstones: Vec<DeletedWaysDecision> = (3..6)
        .map(|n| DeletedWaysDecision {
            decision_id: DecisionId(evidence(&format!("decision-{n}"))),
            set_id: WaysSetId(evidence(&format!("ways-{n}"))),
            deleted_at_unix_ms: n as u64,
            ownership_receipt: evidence("verified-delete-receipt"),
        })
        .collect();
    let identity = canonical.identity().unwrap();
    let legacy = serde_json::to_vec(&SingleFileArchive {
        schema_version: 1,
        journal_id: identity.journal_id(),
        owner: identity.owner(),
        limits: LIMITS,
        records: vec![finished.clone(), unfinished.clone()],
        patches: vec![finished_patch.clone(), unfinished_patch.clone()],
        deleted: tombstones.clone(),
    })
    .unwrap();
    {
        let namespace = namespace(&canonical);
        namespace
            .mark_journal_initialized(WAYS_ARCHIVE_FILE)
            .unwrap();
        namespace.atomic_write(WAYS_ARCHIVE_FILE, &legacy).unwrap();
    }
    let check = |store: &WaysDecisionStore| {
        assert_eq!(store.limits(), LIMITS);
        assert_eq!(
            store.records().unwrap(),
            vec![finished.clone(), unfinished.clone()]
        );
        assert_eq!(store.deleted().unwrap(), tombstones);
        for patch in [&finished_patch, &unfinished_patch] {
            assert_eq!(
                store.patch(&patch.reference).unwrap().unwrap(),
                patch.bytes().unwrap()
            );
        }
    };
    let store = WaysDecisionStore::open_configured_with(namespace(&canonical), &canonical, SMALL)
        .unwrap()
        .unwrap();
    check(&store);
    assert_eq!(store.log.sealed().len(), 1);
    drop(store);

    // The single file is now a head that the earlier store refuses, instead
    // of reading part of the history.
    let primary = component(&canonical).join(WAYS_ARCHIVE_FILE);
    let head = std::fs::read(&primary).unwrap();
    assert!(serde_json::from_slice::<LegacyArchive>(&head).is_err());
    assert!(serde_json::from_slice::<Head>(&head).is_ok());

    // A crash before the head replaced the single file leaves the single file
    // and a partial conversion; opening converts again, exactly once.
    std::fs::write(&primary, &legacy).unwrap();
    std::fs::write(
        component(&canonical)
            .join(EVIDENCE_DIR)
            .join(patch_file(&"a".repeat(64))),
        b"partial",
    )
    .unwrap();
    let mut store = open_small(&canonical);
    check(&store);
    assert_eq!(evidence_files(&canonical).len(), 4);
    let (recreated, patch) = decision(&base, 4, b"again\n");
    assert!(store.freeze(recreated, vec![patch]).is_err());
    delete(&mut store, &finished, 7);
    assert_eq!(store.records().unwrap(), vec![unfinished]);
}

#[test]
fn opening_does_not_rewrite_history() {
    use std::os::unix::fs::MetadataExt;
    let (_root, canonical, base, _) = fixture();
    let mut store = open_small(&canonical);
    let (mut first, first_patch) = decision(&base, 1, b"first\n");
    store.freeze(first.clone(), vec![first_patch]).unwrap();
    settle(&mut store, &mut first, 1);
    let (second, second_patch) = decision(&base, 2, b"second\n");
    store.freeze(second, vec![second_patch]).unwrap();
    drop(store);
    let dir = component(&canonical);
    let mut files = vec![
        dir.join(WAYS_ARCHIVE_FILE),
        dir.join(SMALL.active_name()),
        dir.join("segments").join("ways-decisions.0000000000.jsonl"),
    ];
    files.extend(
        evidence_files(&canonical)
            .into_iter()
            .map(|name| dir.join(EVIDENCE_DIR).join(name)),
    );
    let snapshot = || {
        files
            .iter()
            .map(|path| {
                (
                    std::fs::metadata(path).unwrap().ino(),
                    std::fs::read(path).unwrap(),
                )
            })
            .collect::<Vec<_>>()
    };
    let before = snapshot();
    drop(open_small(&canonical));
    drop(
        WaysDecisionStore::open_configured_with(namespace(&canonical), &canonical, SMALL)
            .unwrap()
            .unwrap(),
    );
    assert_eq!(snapshot(), before);
}

#[test]
fn an_uncertain_index_write_requires_reopening_and_leaves_nothing_behind() {
    let (_root, canonical, base, _) = fixture();
    let mut store = open(&canonical);
    let (first, first_patch) = decision(&base, 1, b"first\n");
    store.freeze(first.clone(), vec![first_patch]).unwrap();
    let active = component(&canonical).join(SPEC.active_name());
    let saved = std::fs::read(&active).unwrap();
    std::fs::remove_file(&active).unwrap();
    std::fs::create_dir(&active).unwrap();
    let (second, second_patch) = decision(&base, 2, b"second\n");
    assert!(store.freeze(second, vec![second_patch.clone()]).is_err());
    assert!(matches!(
        store.records(),
        Err(WaysArchiveError::RecoveryRequired)
    ));
    drop(store);
    std::fs::remove_dir(&active).unwrap();
    std::fs::write(&active, saved).unwrap();

    let store = open(&canonical);
    assert_eq!(store.records().unwrap(), vec![first]);
    assert!(store.patch(&second_patch.reference).unwrap().is_none());
    // The unacknowledged decision's bodies were removed on reopening.
    assert_eq!(evidence_files(&canonical).len(), 2);
}

#[test]
fn retained_bodies_are_checked_against_their_index() {
    let (_root, canonical, record, patch) = fixture();
    let mut store = open(&canonical);
    store.freeze(record.clone(), vec![patch.clone()]).unwrap();
    drop(store);
    let evidence_dir = component(&canonical).join(EVIDENCE_DIR);
    let patch_path = evidence_dir.join(patch_file(&patch.sha256));
    let original = std::fs::read(&patch_path).unwrap();
    let mut changed = original.clone();
    changed[0] ^= 1;
    std::fs::write(&patch_path, &changed).unwrap();
    let store = open(&canonical);
    assert!(store.patch(&patch.reference).is_err());
    drop(store);

    // A missing body or an unexpected entry refuses the store.
    std::fs::remove_file(&patch_path).unwrap();
    let opened = WaysDecisionStore::open_owned(namespace(&canonical), &canonical, LIMITS);
    assert!(opened.is_err());
    std::fs::write(&patch_path, &original).unwrap();
    std::fs::write(evidence_dir.join("unexpected"), b"x").unwrap();
    let opened = WaysDecisionStore::open_owned(namespace(&canonical), &canonical, LIMITS);
    assert!(opened.is_err());
    std::fs::remove_file(evidence_dir.join("unexpected")).unwrap();
    assert_eq!(
        open(&canonical).get(&record.decision_id).unwrap(),
        Some(record)
    );
}

#[test]
fn a_lost_index_is_not_recreated_as_empty() {
    let (_root, canonical, record, patch) = fixture();
    let mut store = open(&canonical);
    store.freeze(record, vec![patch]).unwrap();
    drop(store);
    std::fs::remove_file(component(&canonical).join(SPEC.active_name())).unwrap();
    assert!(WaysDecisionStore::open_owned(namespace(&canonical), &canonical, LIMITS).is_err());
}
